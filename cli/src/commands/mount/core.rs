//! Backend-neutral engine behind `oak mount`.
//!
//! `MountCore` owns everything that is *not* specific to a particular OS
//! virtualization API: the inode tree built from the manifest, the overlay
//! (uncommitted writes), lazy blob hydration (overlay → SQLite cache →
//! remote), and reconciliation with out-of-band `oak commit`s.
//!
//! Each platform backend is a thin adapter over this type:
//!   - Linux  → `fuse_fs.rs` (`impl fuser::Filesystem`)
//!   - macOS  → `fskit` (XPC server forwarding ops from the FSKit extension)
//!   - Windows→ `projfs_fs.rs`
//!
//! Methods return *neutral* shapes — [`NodeAttr`], [`DirEntry`], [`ReadSource`]
//! — and signal failures with raw `libc` errno values, which every backend can
//! map onto its own reply type. Nothing here references `fuser`, FSKit, or
//! ProjFS.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{FileMode, Hash, IgnorePatterns, Manifest, OakError, Result, TreeEntryKind};
use oak_core::{Repository, SqliteRepository};
use tokio::runtime::Handle;

use super::state::{self, DirtyEntry, MountConfig, OverlayMeta};

pub const ROOT_INODE: u64 = 1;

/// Max concurrent off-thread blob hydrations in the read path. Bounds remote
/// pressure during directory-wide read storms (one HTTP fetch per cache-missed
/// file) while staying wide enough to keep a backend's dispatch loop fed.
pub const MAX_CONCURRENT_FETCHES: usize = 8;

/// A cheap fingerprint of `overlay-meta.json` on disk. The mount server is a
/// long-running process, but `oak commit` / `oak status` / `oak push` each run
/// as *separate* processes that mutate the same mount state. After an
/// out-of-band `oak commit` clears the overlay and advances the virtual
/// branch, the server would otherwise keep serving the pre-commit base blobs
/// it captured at mount time. We detect that by stat-ing the overlay metadata
/// file before content reads and reconciling when this signature changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OverlaySig {
    exists: bool,
    len: u64,
    mtime_ns: u128,
}

/// Fingerprint `overlay-meta.json` (existence + length + mtime). A missing
/// file is the all-default signature, so the first write is detected too.
fn overlay_sig(state_dir: &Path) -> OverlaySig {
    match std::fs::metadata(state::overlay_meta_path(state_dir)) {
        Ok(m) => {
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            OverlaySig {
                exists: true,
                len: m.len(),
                mtime_ns,
            }
        }
        Err(_) => OverlaySig::default(),
    }
}

/// What kind of entry an inode represents. Backend-neutral; each adapter maps
/// this onto its own file-type enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
}

/// A backend-neutral stat result. Backends fill in uid/gid/blocks/etc. from
/// this plus their own conventions.
#[derive(Debug, Clone, Copy)]
pub struct NodeAttr {
    pub ino: u64,
    pub size: u64,
    pub kind: EntryKind,
    /// Unix permission bits (e.g. 0o644 / 0o755 / 0o777).
    pub perm: u16,
    pub mtime: SystemTime,
}

/// One directory entry returned by [`MountCore::readdir`]. The `.`/`..`
/// pseudo-entries are included at the front so backends don't have to
/// synthesize them.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub kind: EntryKind,
    pub name: OsString,
}

/// What kind of node a given inode represents (internal tree representation).
#[derive(Debug, Clone)]
enum NodeKind {
    Directory {
        /// Tree object backing this directory, if it comes from the mounted
        /// commit. Overlay-only directories have no base tree.
        base_tree: Option<Hash>,
        /// Whether direct children from `base_tree` have been materialized
        /// into `children`.
        loaded: bool,
    },
    /// A file backed (initially) by a manifest entry.
    File {
        /// Blob hash from the base manifest, or None for files created in
        /// the mount session.
        base_blob: Option<Hash>,
        /// Best-known size: from blob info if available, or from the dirty
        /// overlay file's metadata.
        size: u64,
        mode: FileMode,
    },
}

#[derive(Debug, Clone)]
struct Node {
    ino: u64,
    /// In-mount logical path (forward slashes, no leading `/`). The root
    /// directory has an empty path.
    path: String,
    kind: NodeKind,
    /// Children, by name. Only meaningful for directories.
    children: HashMap<OsString, u64>,
    /// Parent inode (root's parent is itself).
    parent: u64,
}

/// The clonable bundle a backend needs to hydrate a blob off its own dispatch
/// thread. Handed out by [`MountCore::fetch_ctx`].
#[derive(Clone)]
pub struct FetchCtx {
    pub cache: Arc<SqliteRepository>,
    pub remote: String,
    pub owner: String,
    pub repo: String,
    pub token: Option<String>,
    pub sem: Arc<tokio::sync::Semaphore>,
    pub rt: Handle,
}

impl FetchCtx {
    /// Fetch `hash` into the local cache (if missing) and return its bytes.
    /// Bounds concurrency via the shared semaphore. Safe to call from a
    /// spawned task — holds no `&MountCore`.
    pub async fn hydrate(&self, hash: Hash) -> Result<Vec<u8>> {
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|_| OakError::Server("fetch semaphore closed".into()))?;
        super::super::blob_fetch::ensure_blobs_local(
            self.cache.as_ref(),
            &self.remote,
            &self.owner,
            &self.repo,
            self.token.as_deref(),
            std::slice::from_ref(&hash),
        )
        .await?;
        let blob = self
            .cache
            .get_blob(&hash)?
            .ok_or_else(|| OakError::Server(format!("blob {} unavailable after fetch", hash)))?;
        Ok(blob.content)
    }
}

pub struct MountCore {
    /// Inode → Node, protected by a mutex so backend worker threads can mutate.
    inner: Arc<Mutex<Inner>>,
    /// Mount config (immutable for the session).
    cfg: MountConfig,
    /// Bearer token for the remote (looked up from credentials at mount time).
    token: Option<String>,
    /// Tokio runtime handle so we can call async fetch helpers from sync
    /// backend callbacks.
    rt: Handle,
    /// Overlay directory.
    overlay_dir: PathBuf,
    /// Mount state directory (where overlay-meta.json lives).
    state_dir: PathBuf,
    /// Local SQLite cache wrapped in Arc so we can hand it to async tasks.
    cache: Arc<SqliteRepository>,
    /// Sizes known at mount startup. A cold lazy mount may intentionally leave
    /// this empty to avoid fetching every blob's metadata before the mount is
    /// usable; in that case base files without cached metadata stat as size 0
    /// for the mount session.
    base_sizes: HashMap<String, u64>,
    /// Stable mtime for clean (un-dirtied) files and directories — the
    /// timestamp of the base commit we mounted. Returning a *fresh*
    /// `SystemTime::now()` on every getattr/lookup makes editors like vim
    /// think the file changed between open and write.
    base_mtime: SystemTime,
    /// Caps concurrent blob hydrations. Off-thread fetches keep a backend's
    /// dispatch loop responsive, but unbounded fan-out under a read storm
    /// would hit the remote with one request per file at once.
    fetch_sem: Arc<tokio::sync::Semaphore>,
    /// Repository ignore patterns (`.oakignore` / `.gitignore`), loaded once
    /// at mount time. Ignored paths — build output under a gitignored
    /// `target/`, `node_modules/`, etc. — are kept usable on-disk in the
    /// overlay so the filesystem works, but are never recorded as tracked
    /// dirty entries. Recording them would (a) leak them into `oak status` /
    /// `oak commit` and (b) — far worse — force a full `overlay-meta.json`
    /// rewrite on *every* write. A `cargo build` emits tens of thousands of
    /// files into `target/`; with each one re-serializing an ever-growing
    /// overlay manifest, the write path degrades to O(n²) and a single file
    /// create can take many seconds. Skipping ignored paths keeps those
    /// writes O(1).
    ignore: IgnorePatterns,
}

struct Inner {
    /// All inodes, keyed by ino. Inode 1 is the root.
    nodes: HashMap<u64, Node>,
    /// Next ino to allocate.
    next_ino: u64,
    /// Persistent overlay metadata (deletions, dirty entries, renames).
    overlay: OverlayMeta,
    /// Signature of `overlay-meta.json` as this server last wrote or observed
    /// it. A mismatch on the next content read means another process rewrote
    /// it (almost always an out-of-band `oak commit`), so we reconcile.
    last_overlay_sig: OverlaySig,
    /// Virtual-branch head the in-memory tree currently reflects.
    last_head: Option<Hash>,
}

/// Load the repository's ignore rules from the mounted tree. Prefers
/// `.oakignore`, falling back to `.gitignore` — mirroring
/// [`IgnorePatterns::new`], but sourcing the file from the committed blob
/// rather than the working tree (the daemon must not read its own FUSE mount).
/// The blob is hydrated on demand; if it can't be fetched or decoded we fall
/// back to empty patterns (ignore nothing) so the mount still functions — the
/// only cost is losing the build-output fast path until the next mount.
fn load_mount_ignore(
    manifest: &Manifest,
    cache: &SqliteRepository,
    cfg: &MountConfig,
    token: Option<&str>,
    rt: &Handle,
) -> IgnorePatterns {
    let mut ignore_blob: Option<Hash> = None;
    for path in [".oakignore", ".gitignore"] {
        if let Some(entry) = manifest.entries.iter().find(|e| e.path == path) {
            ignore_blob = Some(entry.blob_hash.clone());
            break;
        }
        match cache.resolve_path_in_tree(&manifest.hash, path) {
            Ok(Some(entry)) if entry.kind == TreeEntryKind::Blob => {
                ignore_blob = Some(entry.hash);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(?e, path, "mount: could not resolve ignore file");
                return IgnorePatterns::from_patterns("");
            }
        }
    }

    let Some(ignore_blob) = ignore_blob else {
        return IgnorePatterns::from_patterns("");
    };

    // Hydrate the ignore-file blob if it isn't already in the local cache.
    if cache.get_blob(&ignore_blob).ok().flatten().is_none() {
        // `new()` runs on whatever thread its caller is on. The mount `serve`
        // path constructs us from inside an async fn — i.e. on a tokio worker
        // thread — where `Handle::block_on` panics outright ("Cannot start a
        // runtime from within a runtime"). Setup code and tests, by contrast,
        // call us from a plain thread where `block_on` is fine. Drive this
        // one-off hydrate on a dedicated scratch thread instead: a freshly
        // spawned thread is never a runtime worker, so `block_on` is always
        // legal there regardless of who called `new()`.
        let fetch = std::thread::scope(|s| {
            s.spawn(|| {
                rt.block_on(super::super::blob_fetch::ensure_blobs_local(
                    cache,
                    &cfg.remote_url,
                    &cfg.owner,
                    &cfg.repo,
                    token,
                    std::slice::from_ref(&ignore_blob),
                ))
            })
            .join()
            .expect("ignore-hydrate thread panicked")
        });
        if let Err(e) = fetch {
            tracing::warn!(?e, "mount: could not hydrate ignore file");
            return IgnorePatterns::from_patterns("");
        }
    }

    match cache
        .get_blob(&ignore_blob)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b.content).ok())
    {
        Some(text) => IgnorePatterns::from_patterns(&text),
        None => IgnorePatterns::from_patterns(""),
    }
}

impl MountCore {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: MountConfig,
        cache: Arc<SqliteRepository>,
        manifest: &Manifest,
        sizes: &HashMap<String, u64>,
        token: Option<String>,
        rt: Handle,
        state_dir: PathBuf,
        base_mtime: SystemTime,
    ) -> Result<Self> {
        let overlay_dir = state::overlay_dir(&state_dir);
        std::fs::create_dir_all(&overlay_dir)?;
        let overlay = state::load_overlay_meta(&state_dir)?;

        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                ino: ROOT_INODE,
                path: String::new(),
                kind: NodeKind::Directory {
                    base_tree: Some(manifest.hash.clone()),
                    loaded: false,
                },
                children: HashMap::new(),
                parent: ROOT_INODE,
            },
        );
        let mut next_ino = ROOT_INODE + 1;

        // Base-tree children are materialized lazily by `lookup`/`readdir`.
        // Overlay edits are bounded by local writes, so layering them eagerly
        // preserves active-session state without making startup O(repo files).
        for (old_path, new_path) in &overlay.renames {
            let Some(entry) = cache.resolve_path_in_tree(&manifest.hash, old_path)? else {
                continue;
            };
            if entry.kind != TreeEntryKind::Blob {
                continue;
            }
            let size = sizes.get(entry.hash.as_str()).copied().unwrap_or(0);
            insert_path(
                &mut nodes,
                &mut next_ino,
                new_path,
                NodeKind::File {
                    base_blob: Some(entry.hash),
                    size,
                    mode: entry.mode,
                },
            );
        }

        // Layer in dirty files: any path in the overlay that isn't already in
        // the tree (e.g. a file the user just created). For files that already
        // exist we update their size.
        for (path, dirty) in &overlay.dirty {
            let real_size = std::fs::metadata(overlay_dir.join(&dirty.overlay_file))
                .map(|m| m.len())
                .unwrap_or(0);
            let mode = match dirty.mode.as_str() {
                "executable" => FileMode::Executable,
                "symlink" => FileMode::Symlink,
                _ => FileMode::Regular,
            };
            if let Some(ino) = lookup_path(&nodes, path) {
                if let Some(node) = nodes.get_mut(&ino) {
                    if let NodeKind::File {
                        size: ref mut s,
                        mode: ref mut m,
                        ..
                    } = node.kind
                    {
                        *s = real_size;
                        *m = mode;
                    }
                }
            } else {
                let base_blob = cache
                    .resolve_path_in_tree(&manifest.hash, path)?
                    .and_then(|entry| (entry.kind == TreeEntryKind::Blob).then_some(entry.hash));
                insert_path(
                    &mut nodes,
                    &mut next_ino,
                    path,
                    NodeKind::File {
                        base_blob,
                        size: real_size,
                        mode,
                    },
                );
            }
        }

        // Record the state we're consistent with at mount time so a later
        // out-of-band commit is detectable.
        let last_overlay_sig = overlay_sig(&state_dir);
        let last_head = cache.get_branch_head(&cfg.virtual_branch).ok().flatten();

        // Load the repo's ignore rules from the mounted manifest so the write
        // path can skip recording build output (see the `ignore` field).
        let ignore = load_mount_ignore(manifest, cache.as_ref(), &cfg, token.as_deref(), &rt);

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                nodes,
                next_ino,
                overlay,
                last_overlay_sig,
                last_head,
            })),
            cfg,
            token,
            rt,
            overlay_dir,
            state_dir,
            cache,
            base_sizes: sizes.clone(),
            base_mtime,
            fetch_sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES)),
            ignore,
        })
    }

    /// Whether `path` should be recorded as a tracked dirty entry in the
    /// overlay (and trigger an `overlay-meta.json` rewrite). Ignored paths —
    /// OS metadata and anything matched by `.oakignore` / `.gitignore`, such
    /// as a gitignored `target/` directory — return `false`: their content
    /// still lives in the overlay on disk so reads/writes work within the
    /// session, but they are never recorded as changes. See the `ignore`
    /// field for why this matters for build-tool performance.
    fn is_overlay_tracked(&self, path: &str) -> bool {
        !self.ignore.is_path_or_ancestors_ignored(path)
    }

    /// A clonable fetch context for backends that hydrate blobs off-thread.
    pub fn fetch_ctx(&self) -> FetchCtx {
        FetchCtx {
            cache: self.cache.clone(),
            remote: self.cfg.remote_url.clone(),
            owner: self.cfg.owner.clone(),
            repo: self.cfg.repo.clone(),
            token: self.token.clone(),
            sem: self.fetch_sem.clone(),
            rt: self.rt.clone(),
        }
    }

    /// The mount's stable FS identifier (used in `mount` output / volume name).
    pub fn mount_id(&self) -> &str {
        &self.cfg.id
    }

    // ---- attribute / lookup ---------------------------------------------

    /// Build a neutral attr for a node.
    fn node_attr(&self, node: &Node) -> NodeAttr {
        let mtime = self.mtime_for_node(node);
        match &node.kind {
            NodeKind::Directory { .. } => NodeAttr {
                ino: node.ino,
                size: 0,
                kind: EntryKind::Dir,
                perm: 0o755,
                mtime,
            },
            NodeKind::File { size, mode, .. } => NodeAttr {
                ino: node.ino,
                size: *size,
                kind: match mode {
                    FileMode::Symlink => EntryKind::Symlink,
                    _ => EntryKind::File,
                },
                perm: perm_for(*mode),
                mtime,
            },
        }
    }

    /// Pick the mtime to report for a node. Dirty files use their overlay
    /// file's actual mtime; everything else returns the head commit timestamp.
    fn mtime_for_node(&self, node: &Node) -> SystemTime {
        if let NodeKind::File { .. } = &node.kind {
            let overlay_name = state::overlay_filename_for(&node.path);
            let overlay_path = self.overlay_dir.join(overlay_name);
            if let Ok(meta) = std::fs::metadata(&overlay_path) {
                if let Ok(t) = meta.modified() {
                    return t;
                }
            }
        }
        self.base_mtime
    }

    /// `getattr`: attributes for an inode, or None if it doesn't exist.
    pub fn attr(&self, ino: u64) -> Option<NodeAttr> {
        let inner = self.inner.lock().unwrap();
        inner.nodes.get(&ino).map(|n| self.node_attr(n))
    }

    fn ensure_dir_loaded(&self, inner: &mut Inner, ino: u64) -> std::result::Result<(), i32> {
        let (base_tree, parent_path) = {
            let node = inner.nodes.get(&ino).ok_or(libc::ENOENT)?;
            match &node.kind {
                NodeKind::Directory { base_tree, loaded } => {
                    if *loaded {
                        return Ok(());
                    }
                    (base_tree.clone(), node.path.clone())
                }
                NodeKind::File { .. } => return Err(libc::ENOTDIR),
            }
        };

        let Some(base_tree) = base_tree else {
            if let Some(Node {
                kind: NodeKind::Directory { loaded, .. },
                ..
            }) = inner.nodes.get_mut(&ino)
            {
                *loaded = true;
            }
            return Ok(());
        };

        let tree = self
            .cache
            .get_tree(&base_tree)
            .map_err(|e| {
                tracing::warn!(?e, %base_tree, "mount: failed to load tree");
                libc::EIO
            })?
            .ok_or_else(|| {
                tracing::warn!(%base_tree, "mount: tree missing from cache");
                libc::EIO
            })?;

        let deleted: HashSet<&str> = inner.overlay.deletions.iter().map(String::as_str).collect();
        for entry in tree.entries {
            let original_path = join_path(&parent_path, &entry.name);
            if deleted.contains(original_path.as_str())
                || inner.overlay.renames.contains_key(&original_path)
            {
                continue;
            }
            match entry.kind {
                TreeEntryKind::Tree => {
                    insert_child(
                        &mut inner.nodes,
                        &mut inner.next_ino,
                        ino,
                        OsString::from(&entry.name),
                        original_path,
                        NodeKind::Directory {
                            base_tree: Some(entry.hash),
                            loaded: false,
                        },
                    );
                }
                TreeEntryKind::Blob => {
                    let size = self.base_blob_size(&entry.hash).unwrap_or(0);
                    insert_child(
                        &mut inner.nodes,
                        &mut inner.next_ino,
                        ino,
                        OsString::from(&entry.name),
                        original_path,
                        NodeKind::File {
                            base_blob: Some(entry.hash),
                            size,
                            mode: entry.mode,
                        },
                    );
                }
            }
        }
        if let Some(Node {
            kind: NodeKind::Directory { loaded, .. },
            ..
        }) = inner.nodes.get_mut(&ino)
        {
            *loaded = true;
        }
        Ok(())
    }

    /// `lookup`: resolve `(parent, name)` to a child's attributes.
    pub fn lookup(&self, parent: u64, name: &OsStr) -> Option<NodeAttr> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_dir_loaded(&mut inner, parent).ok()?;
        let parent_node = inner.nodes.get(&parent)?;
        let &child_ino = parent_node.children.get(name)?;
        inner.nodes.get(&child_ino).map(|n| self.node_attr(n))
    }

    /// `readdir`: directory contents including `.` and `..`. Errors with
    /// `ENOENT`/`ENOTDIR`.
    pub fn readdir(&self, ino: u64) -> std::result::Result<Vec<DirEntry>, i32> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_dir_loaded(&mut inner, ino)?;
        let node = inner.nodes.get(&ino).ok_or(libc::ENOENT)?;
        if !matches!(node.kind, NodeKind::Directory { .. }) {
            return Err(libc::ENOTDIR);
        }
        let mut entries: Vec<DirEntry> = Vec::with_capacity(2 + node.children.len());
        entries.push(DirEntry {
            ino: node.ino,
            kind: EntryKind::Dir,
            name: OsString::from("."),
        });
        entries.push(DirEntry {
            ino: node.parent,
            kind: EntryKind::Dir,
            name: OsString::from(".."),
        });
        for (name, &child_ino) in &node.children {
            let kind = match inner.nodes.get(&child_ino).map(|n| &n.kind) {
                Some(NodeKind::Directory { .. }) => EntryKind::Dir,
                Some(NodeKind::File {
                    mode: FileMode::Symlink,
                    ..
                }) => EntryKind::Symlink,
                _ => EntryKind::File,
            };
            entries.push(DirEntry {
                ino: child_ino,
                kind,
                name: name.clone(),
            });
        }
        Ok(entries)
    }

    // ---- read ------------------------------------------------------------

    /// Read the full content for an inode (overlay → cache → remote, the last
    /// one blocking via the runtime handle). Returns None for directories.
    pub fn read_full(&self, ino: u64) -> std::io::Result<Option<Vec<u8>>> {
        let (path, base_blob) = {
            let inner = self.inner.lock().unwrap();
            let node = inner
                .nodes
                .get(&ino)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            match &node.kind {
                NodeKind::Directory { .. } => return Ok(None),
                NodeKind::File { base_blob, .. } => (node.path.clone(), base_blob.clone()),
            }
        };

        // 1. Overlay
        let overlay_name = state::overlay_filename_for(&path);
        let overlay_path = self.overlay_dir.join(&overlay_name);
        if overlay_path.exists() {
            return std::fs::read(&overlay_path).map(Some);
        }

        // 2. Cache → 3. Remote
        let Some(blob_hash) = base_blob else {
            return Ok(Some(Vec::new()));
        };
        let blob = self.fetch_blob(&blob_hash).map_err(io_err)?;
        Ok(Some(blob))
    }

    /// Synchronously fetch a blob from the cache, falling back to the remote
    /// (blocking on the runtime handle).
    fn fetch_blob(&self, hash: &Hash) -> Result<Vec<u8>> {
        if let Some(blob) = self.cache.get_blob(hash)? {
            return Ok(blob.content);
        }
        let ctx = self.fetch_ctx();
        let h = hash.clone();
        self.rt.block_on(async move { ctx.hydrate(h).await })
    }

    /// Resolve where a read should get its bytes WITHOUT touching the network.
    /// Overlay and cache hits return `Ready`; a cache miss with a base blob
    /// returns `Fetch(hash)` for the caller to hydrate off its dispatch
    /// thread (see [`FetchCtx::hydrate`]).
    pub fn read_source(&self, ino: u64) -> std::io::Result<ReadSource> {
        let (path, base_blob) = {
            let inner = self.inner.lock().unwrap();
            let node = inner
                .nodes
                .get(&ino)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            match &node.kind {
                NodeKind::Directory { .. } => return Ok(ReadSource::Dir),
                NodeKind::File { base_blob, .. } => (node.path.clone(), base_blob.clone()),
            }
        };

        // 1. Overlay (writes made this session).
        let overlay_name = state::overlay_filename_for(&path);
        let overlay_path = self.overlay_dir.join(&overlay_name);
        if overlay_path.exists() {
            return std::fs::read(&overlay_path).map(ReadSource::Ready);
        }

        // No base blob and no overlay → empty new file.
        let Some(blob_hash) = base_blob else {
            return Ok(ReadSource::Ready(Vec::new()));
        };

        // 2. Local cache hit → ready immediately, no network.
        if let Some(blob) = self.cache.get_blob(&blob_hash).map_err(io_err)? {
            return Ok(ReadSource::Ready(blob.content));
        }

        // 3. Cache miss → must hit the remote; defer to an off-thread fetch.
        Ok(ReadSource::Fetch(blob_hash))
    }

    /// Fire-and-forget background prefetch of every uncached file blob directly
    /// under `ino`. A `readdir` is a strong signal the caller is about to
    /// `stat`/open those files (editors and Finder do exactly this), so we
    /// batch-hydrate the whole directory in one round-trip instead of paying a
    /// separate cold fetch per file. The subsequent reads then hit the local
    /// cache. Best-effort: errors are logged, never surfaced.
    pub fn prefetch_dir_children(&self, ino: u64) {
        let hashes: Vec<Hash> = {
            let mut inner = self.inner.lock().unwrap();
            if self.ensure_dir_loaded(&mut inner, ino).is_err() {
                return;
            }
            let Some(node) = inner.nodes.get(&ino) else {
                return;
            };
            if !matches!(node.kind, NodeKind::Directory { .. }) {
                return;
            }
            node.children
                .values()
                .filter_map(|child_ino| inner.nodes.get(child_ino))
                .filter_map(|child| match &child.kind {
                    NodeKind::File {
                        base_blob: Some(h), ..
                    } => Some(h.clone()),
                    _ => None,
                })
                .collect()
        };
        // Skip blobs already present locally — no point dispatching a fetch.
        let missing: Vec<Hash> = hashes
            .into_iter()
            .filter(|h| !self.cache.has_blob(h).unwrap_or(false))
            .collect();
        if missing.is_empty() {
            return;
        }
        let ctx = self.fetch_ctx();
        self.rt.spawn(async move {
            if let Err(e) = crate::commands::blob_fetch::ensure_blobs_local(
                ctx.cache.as_ref(),
                &ctx.remote,
                &ctx.owner,
                &ctx.repo,
                ctx.token.as_deref(),
                &missing,
            )
            .await
            {
                tracing::debug!(?e, ino, "fskit: directory prefetch failed");
            }
        });
    }

    // ---- write / create / mutate ----------------------------------------

    /// Materialize a file into the overlay if it isn't already there. The
    /// file's current content is copied first so writes behave like opening
    /// an existing file. Returns the overlay path.
    pub fn materialize_to_overlay(&self, ino: u64) -> std::io::Result<PathBuf> {
        let path = {
            let inner = self.inner.lock().unwrap();
            let node = inner
                .nodes
                .get(&ino)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            node.path.clone()
        };
        let overlay_name = state::overlay_filename_for(&path);
        let overlay_path = self.overlay_dir.join(&overlay_name);
        if !overlay_path.exists() {
            let content = self.read_full(ino)?.unwrap_or_default();
            std::fs::create_dir_all(&self.overlay_dir)?;
            std::fs::write(&overlay_path, &content)?;
        }
        let mode = {
            let inner = self.inner.lock().unwrap();
            match inner.nodes.get(&ino).map(|n| n.kind.clone()) {
                Some(NodeKind::File { mode, .. }) => mode,
                _ => FileMode::Regular,
            }
        };
        // The overlay file on disk is enough for reads/writes within this
        // session (see `read_source`); recording OS metadata or ignored build
        // output in the tracked set would leak it into status / commit and
        // force a full overlay-meta rewrite on every write, so we skip that
        // here (see `is_overlay_tracked`).
        if self.is_overlay_tracked(&path) {
            {
                let mut inner = self.inner.lock().unwrap();
                inner.overlay.dirty.insert(
                    path.clone(),
                    DirtyEntry {
                        overlay_file: overlay_name,
                        mode: mode_str(mode).into(),
                        in_place: false,
                    },
                );
            }
            self.persist_overlay().map_err(io_err)?;
        }
        Ok(overlay_path)
    }

    /// `write`: apply `data` at `offset`, materializing the file first.
    /// Returns the number of bytes written.
    pub fn write(&self, ino: u64, offset: i64, data: &[u8]) -> std::result::Result<u32, i32> {
        let overlay_path = self.materialize_to_overlay(ino).map_err(errno_of)?;
        let result: std::io::Result<u32> = (|| {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&overlay_path)?;
            f.seek(SeekFrom::Start(offset.max(0) as u64))?;
            f.write_all(data)?;
            f.flush()?;
            let new_size = std::fs::metadata(&overlay_path)?.len();
            let mut inner = self.inner.lock().unwrap();
            if let Some(node) = inner.nodes.get_mut(&ino) {
                if let NodeKind::File { size, .. } = &mut node.kind {
                    *size = new_size;
                }
            }
            Ok(data.len() as u32)
        })();
        result.map_err(|e| {
            tracing::warn!(?e, ino, "mount write failed");
            libc::EIO
        })
    }

    /// `create`: make a new empty file under `parent`. `exec` sets the
    /// executable mode bit. Returns the new file's attributes.
    pub fn create(
        &self,
        parent: u64,
        name: &OsStr,
        exec: bool,
    ) -> std::result::Result<NodeAttr, i32> {
        let file_mode = if exec {
            FileMode::Executable
        } else {
            FileMode::Regular
        };
        let name_str = name.to_str().ok_or(libc::EINVAL)?.to_string();

        let new_path = {
            let mut inner = self.inner.lock().unwrap();
            self.ensure_dir_loaded(&mut inner, parent)?;
            let parent_node = inner.nodes.get(&parent).ok_or(libc::ENOENT)?;
            if !matches!(parent_node.kind, NodeKind::Directory { .. }) {
                return Err(libc::ENOTDIR);
            }
            if parent_node.children.contains_key(name) {
                return Err(libc::EEXIST);
            }
            if parent_node.path.is_empty() {
                name_str.clone()
            } else {
                format!("{}/{}", parent_node.path, name_str)
            }
        };

        // Allocate the inode and write an empty overlay file.
        let overlay_name = state::overlay_filename_for(&new_path);
        let overlay_path = self.overlay_dir.join(&overlay_name);
        if let Err(e) = std::fs::write(&overlay_path, b"") {
            tracing::warn!(?e, "create: write empty overlay");
            return Err(libc::EIO);
        }

        let new_ino = {
            let mut inner = self.inner.lock().unwrap();
            let ino = inner.next_ino;
            inner.next_ino += 1;
            inner.nodes.insert(
                ino,
                Node {
                    ino,
                    path: new_path.clone(),
                    kind: NodeKind::File {
                        base_blob: None,
                        size: 0,
                        mode: file_mode,
                    },
                    children: HashMap::new(),
                    parent,
                },
            );
            if let Some(p) = inner.nodes.get_mut(&parent) {
                p.children.insert(name.to_owned(), ino);
            }
            // If this path was previously deleted, undelete it.
            inner.overlay.deletions.retain(|d| d != &new_path);
            // OS metadata (.fseventsd/*, .DS_Store, …) and ignored build
            // output (a gitignored `target/`, `node_modules/`, …) stay usable
            // via their on-disk overlay file but are never recorded as a
            // tracked change (see `is_overlay_tracked`).
            if self.is_overlay_tracked(&new_path) {
                inner.overlay.dirty.insert(
                    new_path.clone(),
                    DirtyEntry {
                        overlay_file: overlay_name,
                        mode: mode_str(file_mode).into(),
                        in_place: false,
                    },
                );
            }
            ino
        };
        if self.is_overlay_tracked(&new_path) {
            if let Err(e) = self.persist_overlay() {
                tracing::warn!(?e, "create: persist overlay-meta");
            }
        }

        let inner = self.inner.lock().unwrap();
        Ok(inner
            .nodes
            .get(&new_ino)
            .map(|n| self.node_attr(n))
            .unwrap_or(NodeAttr {
                ino: new_ino,
                size: 0,
                kind: EntryKind::File,
                perm: perm_for(file_mode),
                mtime: self.base_mtime,
            }))
    }

    /// `mkdir`: create an (in-memory only) directory under `parent`.
    /// Directories aren't first-class in the manifest — they appear implicitly
    /// when files inside them are committed — so this records nothing in the
    /// overlay.
    pub fn mkdir(&self, parent: u64, name: &OsStr) -> std::result::Result<NodeAttr, i32> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_dir_loaded(&mut inner, parent)?;
        let parent_node = inner.nodes.get(&parent).ok_or(libc::ENOENT)?;
        if !matches!(parent_node.kind, NodeKind::Directory { .. }) {
            return Err(libc::ENOTDIR);
        }
        if parent_node.children.contains_key(name) {
            return Err(libc::EEXIST);
        }
        let new_path = if parent_node.path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{}/{}", parent_node.path, name.to_string_lossy())
        };
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.nodes.insert(
            ino,
            Node {
                ino,
                path: new_path,
                kind: NodeKind::Directory {
                    base_tree: None,
                    loaded: true,
                },
                children: HashMap::new(),
                parent,
            },
        );
        if let Some(p) = inner.nodes.get_mut(&parent) {
            p.children.insert(name.to_owned(), ino);
        }
        Ok(NodeAttr {
            ino,
            size: 0,
            kind: EntryKind::Dir,
            perm: 0o755,
            mtime: self.base_mtime,
        })
    }

    /// `unlink`: remove a file. Records a deletion if the file came from the
    /// base manifest; drops the dirty overlay otherwise.
    pub fn unlink(&self, parent: u64, name: &OsStr) -> std::result::Result<(), i32> {
        let path = {
            let mut inner = self.inner.lock().unwrap();
            self.ensure_dir_loaded(&mut inner, parent)?;
            let parent_node = inner.nodes.get(&parent).ok_or(libc::ENOENT)?;
            let &child_ino = parent_node.children.get(name).ok_or(libc::ENOENT)?;
            let child = inner.nodes.get(&child_ino).cloned().ok_or(libc::ENOENT)?;
            if matches!(child.kind, NodeKind::Directory { .. }) {
                return Err(libc::EISDIR);
            }

            if let Some(p) = inner.nodes.get_mut(&parent) {
                p.children.remove(name);
            }
            inner.nodes.remove(&child_ino);

            let was_in_base = matches!(
                child.kind,
                NodeKind::File {
                    base_blob: Some(_),
                    ..
                }
            );
            inner.overlay.dirty.remove(&child.path);
            if was_in_base && !inner.overlay.deletions.contains(&child.path) {
                inner.overlay.deletions.push(child.path.clone());
            }
            child.path
        };
        let overlay_path = self.overlay_dir.join(state::overlay_filename_for(&path));
        if overlay_path.exists() {
            let _ = std::fs::remove_file(&overlay_path);
        }
        self.persist_overlay().map_err(|e| {
            tracing::warn!(?e, "unlink: persist overlay-meta");
            libc::EIO
        })
    }

    /// `rmdir`: remove an empty directory.
    pub fn rmdir(&self, parent: u64, name: &OsStr) -> std::result::Result<(), i32> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_dir_loaded(&mut inner, parent)?;
        let parent_node = inner.nodes.get(&parent).ok_or(libc::ENOENT)?;
        let &child_ino = parent_node.children.get(name).ok_or(libc::ENOENT)?;
        self.ensure_dir_loaded(&mut inner, child_ino)?;
        let child = inner.nodes.get(&child_ino).ok_or(libc::ENOENT)?;
        if !matches!(child.kind, NodeKind::Directory { .. }) {
            return Err(libc::ENOTDIR);
        }
        if !child.children.is_empty() {
            return Err(libc::ENOTEMPTY);
        }
        inner.nodes.remove(&child_ino);
        if let Some(p) = inner.nodes.get_mut(&parent) {
            p.children.remove(name);
        }
        Ok(())
    }

    /// `rename`: move `(parent, name)` to `(newparent, newname)`, updating the
    /// overlay's dirty/rename bookkeeping.
    pub fn rename(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
    ) -> std::result::Result<(), i32> {
        let (old_path, new_path, child_ino) = {
            let mut inner = self.inner.lock().unwrap();
            self.ensure_dir_loaded(&mut inner, parent)?;
            self.ensure_dir_loaded(&mut inner, newparent)?;
            let parent_node = inner.nodes.get(&parent).ok_or(libc::ENOENT)?;
            let &child_ino = parent_node.children.get(name).ok_or(libc::ENOENT)?;
            let child = inner.nodes.get(&child_ino).cloned().ok_or(libc::ENOENT)?;
            let new_parent_node = inner.nodes.get(&newparent).ok_or(libc::ENOENT)?;
            let new_path = if new_parent_node.path.is_empty() {
                newname.to_string_lossy().into_owned()
            } else {
                format!("{}/{}", new_parent_node.path, newname.to_string_lossy())
            };
            let old_path = child.path.clone();

            if let Some(p) = inner.nodes.get_mut(&parent) {
                p.children.remove(name);
            }
            if let Some(np) = inner.nodes.get_mut(&newparent) {
                np.children.insert(newname.to_owned(), child_ino);
            }
            if let Some(node) = inner.nodes.get_mut(&child_ino) {
                node.path = new_path.clone();
                node.parent = newparent;
            }

            if let Some(de) = inner.overlay.dirty.remove(&old_path) {
                let new_overlay = state::overlay_filename_for(&new_path);
                let from = self.overlay_dir.join(&de.overlay_file);
                let to = self.overlay_dir.join(&new_overlay);
                let _ = std::fs::rename(&from, &to);
                inner.overlay.dirty.insert(
                    new_path.clone(),
                    DirtyEntry {
                        overlay_file: new_overlay,
                        mode: de.mode,
                        in_place: de.in_place,
                    },
                );
            }
            let in_base = matches!(
                child.kind,
                NodeKind::File {
                    base_blob: Some(_),
                    ..
                }
            );
            if in_base {
                inner
                    .overlay
                    .renames
                    .insert(old_path.clone(), new_path.clone());
            }
            (old_path, new_path, child_ino)
        };
        self.persist_overlay().map_err(|e| {
            tracing::warn!(?e, "rename: persist overlay-meta");
            libc::EIO
        })?;
        tracing::debug!(%old_path, %new_path, ino = child_ino, "rename");
        Ok(())
    }

    /// `setattr(size=)`: truncate (or extend) a file to `new_size`. Returns the
    /// updated attributes. Other setattr fields (mode/uid/times) are read-only
    /// in this filesystem and ignored by callers.
    pub fn truncate(&self, ino: u64, new_size: u64) -> std::result::Result<NodeAttr, i32> {
        let overlay_path = self.materialize_to_overlay(ino).map_err(errno_of)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&overlay_path)
            .and_then(|f| f.set_len(new_size))
            .map_err(|e| {
                tracing::warn!(?e, "setattr: truncate failed");
                libc::EIO
            })?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(node) = inner.nodes.get_mut(&ino) {
            if let NodeKind::File { size, .. } = &mut node.kind {
                *size = new_size;
            }
        }
        inner
            .nodes
            .get(&ino)
            .map(|n| self.node_attr(n))
            .ok_or(libc::ENOENT)
    }

    // ---- overlay persistence / reconciliation ---------------------------

    pub fn persist_overlay(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        // Cross-process lock shared with `oak commit`'s overlay clear, so the
        // two writes can't interleave (the daemon overwriting a fresh clear
        // would resurrect just-committed entries as phantom dirty files).
        let _lock = state::lock_overlay_meta(&self.state_dir)?;
        state::save_overlay_meta(&self.state_dir, &inner.overlay)?;
        inner.last_overlay_sig = overlay_sig(&self.state_dir);
        Ok(())
    }

    /// Cheaply detect, and reconcile with, an out-of-band change to the mount
    /// state — usually `oak commit` running in another process. When the
    /// on-disk overlay signature is unchanged this does a single `stat` and
    /// returns. Backends should call this at the start of read-side ops.
    pub fn reconcile_if_external_change(&self) {
        let sig = overlay_sig(&self.state_dir);
        let mut inner = self.inner.lock().unwrap();
        if inner.last_overlay_sig == sig {
            return;
        }
        if let Err(e) = self.reconcile_locked(&mut inner) {
            tracing::warn!(?e, "mount: reconcile after external overlay change failed");
        }
        inner.last_overlay_sig = sig;
    }

    /// Rebuild in-memory base blobs/sizes from the current virtual-branch head
    /// manifest. Called with `inner` already locked.
    fn reconcile_locked(&self, inner: &mut Inner) -> Result<()> {
        inner.overlay = state::load_overlay_meta(&self.state_dir)?;

        let head = self.cache.get_branch_head(&self.cfg.virtual_branch)?;
        if head == inner.last_head {
            return Ok(());
        }
        let Some(head) = head else {
            return Ok(());
        };
        let commit = self
            .cache
            .get_commit(&head)?
            .ok_or_else(|| OakError::Server("reconcile: head commit missing".into()))?;
        let manifest = self
            .cache
            .get_manifest(&commit.manifest_hash)?
            .ok_or_else(|| OakError::Server("reconcile: head manifest missing".into()))?;

        let renames = inner.overlay.renames.clone();
        let deleted: HashSet<String> = inner.overlay.deletions.iter().cloned().collect();
        let dirty: HashSet<String> = inner.overlay.dirty.keys().cloned().collect();

        let mut want: HashMap<String, (Hash, FileMode)> = HashMap::new();
        for entry in &manifest.entries {
            if deleted.contains(&entry.path) {
                continue;
            }
            let display = renames
                .get(&entry.path)
                .cloned()
                .unwrap_or_else(|| entry.path.clone());
            want.insert(display, (entry.blob_hash.clone(), entry.mode));
        }

        for (path, (blob, mode)) in &want {
            if dirty.contains(path) {
                continue;
            }
            match lookup_path(&inner.nodes, path) {
                Some(ino) => {
                    let unchanged = matches!(
                        inner.nodes.get(&ino).map(|n| &n.kind),
                        Some(NodeKind::File { base_blob, .. }) if base_blob.as_ref() == Some(blob)
                    );
                    if unchanged {
                        continue;
                    }
                    let size = self.base_blob_size(blob).unwrap_or(0);
                    if let Some(NodeKind::File {
                        base_blob,
                        size: s,
                        mode: m,
                    }) = inner.nodes.get_mut(&ino).map(|n| &mut n.kind)
                    {
                        *base_blob = Some(blob.clone());
                        *s = size;
                        *m = *mode;
                    }
                }
                None => {
                    let size = self.base_blob_size(blob).unwrap_or(0);
                    let kind = NodeKind::File {
                        base_blob: Some(blob.clone()),
                        size,
                        mode: *mode,
                    };
                    insert_path(&mut inner.nodes, &mut inner.next_ino, path, kind);
                }
            }
        }

        let stale: Vec<u64> = inner
            .nodes
            .iter()
            .filter(|(&ino, node)| {
                ino != ROOT_INODE
                    && matches!(node.kind, NodeKind::File { .. })
                    && !want.contains_key(&node.path)
                    && !dirty.contains(&node.path)
            })
            .map(|(&ino, _)| ino)
            .collect();
        for ino in stale {
            remove_node(&mut inner.nodes, ino);
        }

        inner.last_head = Some(head);
        Ok(())
    }

    /// Size for a committed base blob when serving attributes. Prefer the
    /// startup size map. When that map is empty, the mount deliberately skipped
    /// all-repo blob metadata prefetch, so avoid a per-file SQLite miss storm
    /// while hydrating wide directories.
    fn base_blob_size(&self, hash: &Hash) -> Option<u64> {
        if let Some(size) = self.base_sizes.get(hash.as_str()) {
            return Some(*size);
        }
        if self.base_sizes.is_empty() {
            return None;
        }
        self.blob_size(hash)
    }

    /// Best-effort size of a (locally-cached) blob without loading its full
    /// content: sum the chunk lengths, falling back to the blob record.
    fn blob_size(&self, hash: &Hash) -> Option<u64> {
        if let Ok(Some(chunks)) = self.cache.get_blob_chunks(hash) {
            if !chunks.is_empty() {
                return Some(chunks.iter().map(|c| c.length as u64).sum());
            }
        }
        self.cache.get_blob(hash).ok().flatten().map(|b| b.size)
    }
}

/// Where a `read` gets its bytes. Resolved without the network; only `Fetch`
/// needs the remote, hydrated off the dispatch thread via [`FetchCtx`].
pub enum ReadSource {
    /// The inode is a directory (read should return EISDIR).
    Dir,
    /// Content available locally (overlay hit, cache hit, or empty new file).
    Ready(Vec<u8>),
    /// Cache miss with a base blob — fetch this hash from the remote.
    Fetch(Hash),
}

/// Slice `[offset, offset + size)` out of `content`, clamped to its length.
/// Shared by backends that answer reads with a byte range.
pub fn slice_range(content: &[u8], offset: i64, size: u32) -> &[u8] {
    let start = offset.max(0) as usize;
    if start >= content.len() {
        return &[];
    }
    let end = (start + size as usize).min(content.len());
    &content[start..end]
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn insert_child(
    nodes: &mut HashMap<u64, Node>,
    next_ino: &mut u64,
    parent_ino: u64,
    name: OsString,
    path: String,
    kind: NodeKind,
) -> u64 {
    if let Some(existing) = nodes
        .get(&parent_ino)
        .and_then(|parent| parent.children.get(&name))
        .copied()
    {
        if let Some(node) = nodes.get_mut(&existing) {
            match (&mut node.kind, kind) {
                (
                    NodeKind::Directory { base_tree, loaded },
                    NodeKind::Directory {
                        base_tree: new_base,
                        ..
                    },
                ) => {
                    if new_base.is_some() && *base_tree != new_base {
                        *base_tree = new_base;
                        *loaded = false;
                    }
                }
                (
                    NodeKind::File {
                        base_blob, mode, ..
                    },
                    NodeKind::File {
                        base_blob: new_base,
                        mode: new_mode,
                        ..
                    },
                ) => {
                    if base_blob.is_none() {
                        *base_blob = new_base;
                    }
                    *mode = new_mode;
                }
                _ => {}
            }
        }
        return existing;
    }

    let ino = *next_ino;
    *next_ino += 1;
    nodes.insert(
        ino,
        Node {
            ino,
            path,
            kind,
            children: HashMap::new(),
            parent: parent_ino,
        },
    );
    if let Some(parent) = nodes.get_mut(&parent_ino) {
        parent.children.insert(name, ino);
    }
    ino
}

/// Walk the tree, creating directory nodes as needed, and place a file or
/// directory at the given relative path.
fn insert_path(nodes: &mut HashMap<u64, Node>, next_ino: &mut u64, path: &str, kind: NodeKind) {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    let mut parent_ino = ROOT_INODE;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        let name = OsString::from(*part);

        if let Some(&child_ino) = nodes.get(&parent_ino).and_then(|n| n.children.get(&name)) {
            if is_last {
                return; // already present
            }
            parent_ino = child_ino;
            continue;
        }

        let ino = *next_ino;
        *next_ino += 1;

        let child_path = if is_last {
            path.to_string()
        } else {
            parts[..=i].join("/")
        };
        let child_kind = if is_last {
            kind.clone()
        } else {
            NodeKind::Directory {
                base_tree: None,
                loaded: true,
            }
        };

        nodes.insert(
            ino,
            Node {
                ino,
                path: child_path,
                kind: child_kind,
                children: HashMap::new(),
                parent: parent_ino,
            },
        );
        if let Some(parent) = nodes.get_mut(&parent_ino) {
            parent.children.insert(name, ino);
        }
        parent_ino = ino;
    }
}

fn lookup_path(nodes: &HashMap<u64, Node>, path: &str) -> Option<u64> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut cur = ROOT_INODE;
    for part in parts {
        let name = OsString::from(part);
        let next = nodes.get(&cur)?.children.get(&name).copied()?;
        cur = next;
    }
    Some(cur)
}

/// Detach a file node from its parent and drop it from the inode table.
fn remove_node(nodes: &mut HashMap<u64, Node>, ino: u64) {
    let Some((parent, name)) = nodes.get(&ino).map(|n| {
        let leaf = n.path.rsplit('/').next().unwrap_or(n.path.as_str());
        (n.parent, OsString::from(leaf))
    }) else {
        return;
    };
    if let Some(p) = nodes.get_mut(&parent) {
        p.children.remove(&name);
    }
    nodes.remove(&ino);
}

fn perm_for(mode: FileMode) -> u16 {
    match mode {
        FileMode::Executable => 0o755,
        FileMode::Symlink => 0o777,
        FileMode::Regular => 0o644,
    }
}

fn mode_str(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Executable => "executable",
        FileMode::Symlink => "symlink",
        FileMode::Regular => "regular",
    }
}

fn io_err(e: OakError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Map an `io::Error` to a libc errno, defaulting to EIO.
fn errno_of(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (HashMap<u64, Node>, u64) {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                ino: ROOT_INODE,
                path: String::new(),
                kind: NodeKind::Directory {
                    base_tree: None,
                    loaded: true,
                },
                children: HashMap::new(),
                parent: ROOT_INODE,
            },
        );
        (nodes, ROOT_INODE + 1)
    }

    fn file_kind() -> NodeKind {
        NodeKind::File {
            base_blob: Some(Hash("h".into())),
            size: 0,
            mode: FileMode::Regular,
        }
    }

    #[test]
    fn insert_path_creates_intermediate_dirs() {
        let (mut nodes, mut next_ino) = fresh();
        insert_path(&mut nodes, &mut next_ino, "src/utils/io.rs", file_kind());

        assert_eq!(nodes.len(), 4);

        let src_ino = lookup_path(&nodes, "src").expect("src dir");
        let utils_ino = lookup_path(&nodes, "src/utils").expect("utils dir");
        let leaf_ino = lookup_path(&nodes, "src/utils/io.rs").expect("file");

        assert!(matches!(nodes[&src_ino].kind, NodeKind::Directory { .. }));
        assert!(matches!(nodes[&utils_ino].kind, NodeKind::Directory { .. }));
        assert!(matches!(nodes[&leaf_ino].kind, NodeKind::File { .. }));

        assert_eq!(nodes[&utils_ino].parent, src_ino);
        assert_eq!(nodes[&leaf_ino].parent, utils_ino);
    }

    #[test]
    fn insert_path_reuses_existing_dirs() {
        let (mut nodes, mut next_ino) = fresh();
        insert_path(&mut nodes, &mut next_ino, "src/a.rs", file_kind());
        insert_path(&mut nodes, &mut next_ino, "src/b.rs", file_kind());

        let src_ino = lookup_path(&nodes, "src").expect("one src dir");
        let src = &nodes[&src_ino];
        assert_eq!(src.children.len(), 2);
        assert!(src.children.contains_key(std::ffi::OsStr::new("a.rs")));
        assert!(src.children.contains_key(std::ffi::OsStr::new("b.rs")));
    }

    #[test]
    fn insert_path_idempotent_on_duplicate() {
        let (mut nodes, mut next_ino) = fresh();
        insert_path(&mut nodes, &mut next_ino, "src/a.rs", file_kind());
        let count = nodes.len();
        insert_path(&mut nodes, &mut next_ino, "src/a.rs", file_kind());
        assert_eq!(nodes.len(), count, "duplicate insert is a no-op");
    }

    #[test]
    fn insert_path_handles_top_level_file() {
        let (mut nodes, mut next_ino) = fresh();
        insert_path(&mut nodes, &mut next_ino, "README.md", file_kind());
        assert_eq!(nodes.len(), 2);
        let leaf = lookup_path(&nodes, "README.md").expect("top-level file");
        assert_eq!(nodes[&leaf].parent, ROOT_INODE);
    }

    #[test]
    fn lookup_path_missing_returns_none() {
        let (nodes, _) = fresh();
        assert_eq!(lookup_path(&nodes, "does/not/exist"), None);
    }

    #[test]
    fn slice_range_clamps() {
        assert_eq!(slice_range(b"hello", 0, 3), b"hel");
        assert_eq!(slice_range(b"hello", 3, 10), b"lo");
        assert_eq!(slice_range(b"hello", 10, 3), b"");
        assert_eq!(slice_range(b"hello", -1, 2), b"he");
    }

    use oak_core::{Blob, FileChange, ManifestEntry};
    use tempfile::TempDir;

    /// Build a `MountCore` over a temp SQLite cache whose virtual branch `vb`
    /// has a single commit containing `files`.
    fn build_core(
        files: &[(&str, &[u8])],
    ) -> (
        MountCore,
        Arc<SqliteRepository>,
        tokio::runtime::Runtime,
        TempDir,
    ) {
        build_core_with_size_map(files, true)
    }

    fn build_core_with_size_map(
        files: &[(&str, &[u8])],
        include_sizes: bool,
    ) -> (
        MountCore,
        Arc<SqliteRepository>,
        tokio::runtime::Runtime,
        TempDir,
    ) {
        build_core_with_options(files, include_sizes, false)
    }

    fn build_core_with_options(
        files: &[(&str, &[u8])],
        include_sizes: bool,
        root_only_manifest: bool,
    ) -> (
        MountCore,
        Arc<SqliteRepository>,
        tokio::runtime::Runtime,
        TempDir,
    ) {
        let dir = TempDir::new().unwrap();
        let state_dir = dir.path().to_path_buf();
        let cache = Arc::new(SqliteRepository::open(&state_dir.join("cache.db")).unwrap());

        let manifest = store_commit(&cache, None, files);
        cache.set_branch_head("vb", &head_of(&cache)).unwrap();

        let cfg = MountConfig {
            id: "testid".into(),
            mount_point: state_dir.join("mnt"),
            remote_url: "https://example.invalid".into(),
            owner: "o".into(),
            repo: "r".into(),
            base_branch: "main".into(),
            base_commit: head_of(&cache).as_str().to_string(),
            virtual_branch: "vb".into(),
        };
        let mut sizes = HashMap::new();
        if include_sizes {
            for e in &manifest.entries {
                let b = cache.get_blob(&e.blob_hash).unwrap().unwrap();
                sizes.insert(e.blob_hash.as_str().to_string(), b.size);
            }
        }
        let mount_manifest = if root_only_manifest {
            Manifest {
                hash: manifest.hash.clone(),
                entries: Vec::new(),
            }
        } else {
            manifest
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let core = MountCore::new(
            cfg,
            cache.clone(),
            &mount_manifest,
            &sizes,
            None,
            rt.handle().clone(),
            state_dir,
            SystemTime::now(),
        )
        .unwrap();
        (core, cache, rt, dir)
    }

    fn head_of(cache: &SqliteRepository) -> Hash {
        cache.get_branch_head("vb").unwrap().unwrap()
    }

    fn store_commit(
        cache: &SqliteRepository,
        parent: Option<Hash>,
        files: &[(&str, &[u8])],
    ) -> Manifest {
        let mut entries = Vec::new();
        for (path, content) in files {
            let blob = Blob::new(content.to_vec());
            cache.store_blob(&blob).unwrap();
            entries.push(ManifestEntry {
                path: (*path).to_string(),
                blob_hash: blob.hash.clone(),
                mode: FileMode::Regular,
            });
        }
        let manifest = Manifest::new(entries);
        let mh = cache.put_manifest(manifest.entries.clone()).unwrap();
        let no_files: Vec<FileChange> = Vec::new();
        let head = cache
            .put_commit(
                "vb".into(),
                parent,
                None,
                mh,
                "tester".into(),
                None,
                chrono::Utc::now(),
                no_files,
            )
            .unwrap();
        cache.set_branch_head("vb", &head).unwrap();
        manifest
    }

    /// Simulate an out-of-band `oak commit`: advance `vb` to a new manifest and
    /// clear the on-disk overlay metadata, exactly as the real commit path does.
    fn external_commit(cache: &SqliteRepository, state_dir: &Path, files: &[(&str, &[u8])]) {
        let parent = cache.get_branch_head("vb").unwrap();
        store_commit(cache, parent, files);
        state::save_overlay_meta(state_dir, &OverlayMeta::default()).unwrap();
    }

    fn ino_of(core: &MountCore, path: &str) -> Option<u64> {
        if path.is_empty() {
            return Some(ROOT_INODE);
        }
        let mut cur = ROOT_INODE;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            cur = core.lookup(cur, OsStr::new(part))?.ino;
        }
        Some(cur)
    }

    fn node_size(core: &MountCore, ino: u64) -> u64 {
        let inner = core.inner.lock().unwrap();
        match &inner.nodes.get(&ino).unwrap().kind {
            NodeKind::File { size, .. } => *size,
            _ => panic!("not a file"),
        }
    }

    fn is_dirty(core: &MountCore, path: &str) -> bool {
        core.inner.lock().unwrap().overlay.dirty.contains_key(path)
    }

    /// Regression test for the cargo-in-a-mount slowdown: files matched by the
    /// repo's `.gitignore` (e.g. everything under a build `target/`) must never
    /// be recorded as tracked overlay entries. Recording them forced a full
    /// `overlay-meta.json` rewrite on every create/write, turning a build that
    /// emits tens of thousands of artifacts into an O(n²) crawl.
    #[test]
    fn ignored_build_output_is_not_recorded_in_overlay() {
        use std::ffi::OsStr;
        let (core, _cache, _rt, _dir) = build_core(&[
            (".gitignore", b"/target/\n"),
            ("src/main.rs", b"fn main() {}\n"),
        ]);

        // A normal new file under a tracked dir IS recorded.
        let src = ino_of(&core, "src").expect("src dir present");
        core.create(src, OsStr::new("new.rs"), false).unwrap();
        assert!(
            is_dirty(&core, "src/new.rs"),
            "a normal new file must be recorded as a dirty overlay entry"
        );

        // A file created under the gitignored `target/` is NOT recorded...
        let target = core.mkdir(ROOT_INODE, OsStr::new("target")).unwrap().ino;
        let obj = core.create(target, OsStr::new("app.o"), false).unwrap().ino;
        assert!(
            !is_dirty(&core, "target/app.o"),
            "gitignored build output must not be recorded in the overlay"
        );

        // ...yet it is still a live, writable node within the session.
        core.write(obj, 0, b"OBJ").unwrap();
        assert_eq!(core.read_full(obj).unwrap().unwrap(), b"OBJ");
        assert!(
            !is_dirty(&core, "target/app.o"),
            "writing ignored output must not record a dirty entry either"
        );

        // The whole point: the overlay manifest stays empty of build output,
        // so persisting it never balloons.
        assert!(
            core.inner
                .lock()
                .unwrap()
                .overlay
                .dirty
                .keys()
                .all(|p| !p.starts_with("target/")),
            "no target/ paths should ever land in the overlay"
        );
    }

    #[test]
    fn root_only_manifest_still_loads_ignore_rules_from_tree() {
        use std::ffi::OsStr;
        let (core, _cache, _rt, _dir) = build_core_with_options(
            &[
                (".gitignore", b"/target/\n"),
                ("src/main.rs", b"fn main() {}\n"),
            ],
            false,
            true,
        );

        let src = core
            .lookup(ROOT_INODE, OsStr::new("src"))
            .expect("src should hydrate from root tree");
        core.create(src.ino, OsStr::new("new.rs"), false).unwrap();
        assert!(
            is_dirty(&core, "src/new.rs"),
            "tracked paths should still be recorded with a root-only startup manifest"
        );

        let target = core.mkdir(ROOT_INODE, OsStr::new("target")).unwrap().ino;
        let obj = core.create(target, OsStr::new("app.o"), false).unwrap().ino;
        core.write(obj, 0, b"OBJ").unwrap();
        assert_eq!(core.read_full(obj).unwrap().unwrap(), b"OBJ");
        assert!(
            !is_dirty(&core, "target/app.o"),
            "root-only startup must still honor committed .gitignore rules"
        );
    }

    #[test]
    fn reconcile_follows_out_of_band_commit() {
        let (core, cache, _rt, dir) = build_core(&[("foo.txt", b"BASE\n"), ("gone.txt", b"X\n")]);
        let state_dir = dir.path().to_path_buf();

        let foo = ino_of(&core, "foo.txt").expect("foo present");
        assert_eq!(core.read_full(foo).unwrap().unwrap(), b"BASE\n");
        assert!(ino_of(&core, "gone.txt").is_some());

        const NEW: &[u8] = b"NEW CONTENT, LONGER THAN BASE\n";
        external_commit(
            &cache,
            &state_dir,
            &[("foo.txt", NEW), ("added.txt", b"added\n")],
        );

        core.reconcile_if_external_change();

        assert_eq!(
            core.read_full(foo).unwrap().unwrap(),
            NEW,
            "read must follow committed content, not revert to base"
        );
        assert_eq!(
            node_size(&core, foo),
            NEW.len() as u64,
            "reported size must match committed content"
        );

        let added = ino_of(&core, "added.txt").expect("added.txt should appear");
        assert_eq!(core.read_full(added).unwrap().unwrap(), b"added\n");
        assert!(
            ino_of(&core, "gone.txt").is_none(),
            "committed deletion should drop the node"
        );
    }

    #[test]
    fn reconcile_preserves_dirty_overlay() {
        let (core, cache, _rt, dir) = build_core(&[("foo.txt", b"BASE\n")]);
        let state_dir = dir.path().to_path_buf();

        let overlay_dir = state::overlay_dir(&state_dir);
        std::fs::create_dir_all(&overlay_dir).unwrap();
        std::fs::write(
            overlay_dir.join(state::overlay_filename_for("foo.txt")),
            b"MY EDIT\n",
        )
        .unwrap();
        {
            let mut inner = core.inner.lock().unwrap();
            inner.overlay.dirty.insert(
                "foo.txt".into(),
                DirtyEntry {
                    overlay_file: state::overlay_filename_for("foo.txt"),
                    mode: "regular".into(),
                    in_place: false,
                },
            );
        }
        core.persist_overlay().unwrap();

        external_commit(
            &cache,
            &state_dir,
            &[("foo.txt", b"BASE\n"), ("other.txt", b"o\n")],
        );
        {
            let mut inner = core.inner.lock().unwrap();
            inner.overlay.dirty.insert(
                "foo.txt".into(),
                DirtyEntry {
                    overlay_file: state::overlay_filename_for("foo.txt"),
                    mode: "regular".into(),
                    in_place: false,
                },
            );
        }
        core.persist_overlay().unwrap();
        core.reconcile_if_external_change();

        let foo = ino_of(&core, "foo.txt").unwrap();
        assert_eq!(
            core.read_full(foo).unwrap().unwrap(),
            b"MY EDIT\n",
            "dirty overlay content must survive an out-of-band reconcile"
        );
    }

    #[test]
    fn reconcile_is_noop_without_external_change() {
        let (core, _cache, _rt, _dir) = build_core(&[("foo.txt", b"BASE\n")]);
        let foo = ino_of(&core, "foo.txt").unwrap();
        core.reconcile_if_external_change();
        core.reconcile_if_external_change();
        assert_eq!(core.read_full(foo).unwrap().unwrap(), b"BASE\n");
    }

    #[test]
    fn new_defers_manifest_inode_materialization_until_directory_read() {
        let owned: Vec<(String, Vec<u8>)> = (0..1_000)
            .map(|i| (format!("src/file-{i:04}.txt"), b"x".to_vec()))
            .collect();
        let files: Vec<(&str, &[u8])> = owned
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_slice()))
            .collect();
        let (core, _cache, _rt, _dir) = build_core(&files);

        assert_eq!(
            core.inner.lock().unwrap().nodes.len(),
            1,
            "startup should materialize only the root inode"
        );

        let root = core.readdir(ROOT_INODE).expect("root readdir");
        assert!(
            root.iter().any(|entry| entry.name == OsStr::new("src")),
            "root readdir should expose the top-level directory"
        );
        assert_eq!(
            core.inner.lock().unwrap().nodes.len(),
            2,
            "root readdir should not materialize every file below src/"
        );

        let src = core
            .lookup(ROOT_INODE, OsStr::new("src"))
            .expect("src lookup");
        let src_entries = core.readdir(src.ino).expect("src readdir");
        assert!(
            src_entries
                .iter()
                .any(|entry| entry.name == OsStr::new("file-0000.txt")),
            "reading src should hydrate that directory's direct children"
        );
        assert!(
            core.inner.lock().unwrap().nodes.len() >= 1_002,
            "only the demanded directory should be hydrated"
        );
    }

    #[test]
    fn directory_hydration_tolerates_deferred_base_sizes() {
        let (core, _cache, _rt, _dir) =
            build_core_with_size_map(&[("src/foo.txt", b"hello")], false);

        let src = core
            .lookup(ROOT_INODE, OsStr::new("src"))
            .expect("src lookup");
        let src_entries = core.readdir(src.ino).expect("src readdir");
        assert!(
            src_entries
                .iter()
                .any(|entry| entry.name == OsStr::new("foo.txt")),
            "directory hydration should not require startup blob sizes"
        );

        let foo = core
            .lookup(src.ino, OsStr::new("foo.txt"))
            .expect("foo lookup");
        assert_eq!(
            foo.size, 0,
            "a cold lazy mount reports unknown base-blob sizes as 0"
        );
        assert_eq!(
            core.read_full(foo.ino).unwrap().unwrap(),
            b"hello",
            "content reads still use the cached blob even when size metadata was deferred"
        );
    }

    #[test]
    fn overlay_sig_changes_when_meta_written() {
        let dir = TempDir::new().unwrap();
        let before = overlay_sig(dir.path());
        assert!(!before.exists);
        state::save_overlay_meta(dir.path(), &OverlayMeta::default()).unwrap();
        let after = overlay_sig(dir.path());
        assert!(after.exists);
        assert_ne!(before, after);
    }

    // ---- op-level coverage on the neutral API ---------------------------

    #[test]
    fn create_write_read_roundtrip() {
        let (core, _cache, _rt, _dir) = build_core(&[("foo.txt", b"BASE\n")]);
        let attr = core
            .create(ROOT_INODE, OsStr::new("new.txt"), false)
            .expect("create");
        assert_eq!(attr.kind, EntryKind::File);
        let n = core.write(attr.ino, 0, b"hello world").expect("write");
        assert_eq!(n, 11);
        assert_eq!(core.read_full(attr.ino).unwrap().unwrap(), b"hello world");
        assert_eq!(core.attr(attr.ino).unwrap().size, 11);
    }

    #[test]
    fn os_metadata_is_usable_but_untracked() {
        // Reproduces macOS writing `.fseventsd/fseventsd-uuid` into a fresh
        // mount: the file must work in-session but never appear in status.
        let (core, _cache, _rt, dir) = build_core(&[("foo.txt", b"BASE\n")]);
        let fsd = core
            .mkdir(ROOT_INODE, OsStr::new(".fseventsd"))
            .expect("mkdir");
        let attr = core
            .create(fsd.ino, OsStr::new("fseventsd-uuid"), false)
            .expect("create");
        let n = core.write(attr.ino, 0, b"ABCD-1234").expect("write");
        assert_eq!(n, 9);
        // Reads still resolve from the overlay file on disk.
        assert_eq!(core.read_full(attr.ino).unwrap().unwrap(), b"ABCD-1234");
        // ...but nothing is recorded as a tracked change.
        {
            let inner = core.inner.lock().unwrap();
            assert!(inner.overlay.dirty.is_empty(), "in-memory overlay clean");
        }
        let meta = state::load_overlay_meta(dir.path()).unwrap();
        assert!(meta.dirty.is_empty(), "persisted overlay clean");
    }

    #[test]
    fn load_overlay_meta_strips_legacy_os_metadata() {
        // An overlay persisted by an older build (before the create-time guard)
        // is cleaned on load so stale junk never resurfaces in status.
        let dir = TempDir::new().unwrap();
        let mut meta = OverlayMeta::default();
        meta.dirty.insert(
            ".fseventsd/fseventsd-uuid".into(),
            DirtyEntry {
                overlay_file: "x".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        meta.dirty.insert(
            "src/main.rs".into(),
            DirtyEntry {
                overlay_file: "y".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        meta.deletions.push(".DS_Store".into());
        state::save_overlay_meta(dir.path(), &meta).unwrap();

        let loaded = state::load_overlay_meta(dir.path()).unwrap();
        assert_eq!(loaded.dirty.len(), 1);
        assert!(loaded.dirty.contains_key("src/main.rs"));
        assert!(loaded.deletions.is_empty());
    }

    #[test]
    fn unlink_records_deletion_for_base_file() {
        let (core, _cache, _rt, dir) = build_core(&[("foo.txt", b"BASE\n")]);
        let foo = ino_of(&core, "foo.txt").unwrap();
        core.unlink(ROOT_INODE, OsStr::new("foo.txt"))
            .expect("unlink");
        assert!(ino_of(&core, "foo.txt").is_none());
        let meta = state::load_overlay_meta(dir.path()).unwrap();
        assert!(meta.deletions.contains(&"foo.txt".to_string()));
        let _ = foo;
    }

    #[test]
    fn rename_base_file_records_rename() {
        let (core, _cache, _rt, dir) = build_core(&[("foo.txt", b"BASE\n")]);
        core.rename(
            ROOT_INODE,
            OsStr::new("foo.txt"),
            ROOT_INODE,
            OsStr::new("bar.txt"),
        )
        .expect("rename");
        assert!(ino_of(&core, "foo.txt").is_none());
        assert!(ino_of(&core, "bar.txt").is_some());
        let meta = state::load_overlay_meta(dir.path()).unwrap();
        assert_eq!(meta.renames.get("foo.txt"), Some(&"bar.txt".to_string()));
    }

    #[test]
    fn readdir_lists_dot_and_children() {
        let (core, _cache, _rt, _dir) = build_core(&[("a.txt", b"1"), ("b.txt", b"2")]);
        let entries = core.readdir(ROOT_INODE).expect("readdir");
        let names: HashSet<String> = entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains("."));
        assert!(names.contains(".."));
        assert!(names.contains("a.txt"));
        assert!(names.contains("b.txt"));
    }

    #[test]
    fn truncate_shrinks_file() {
        let (core, _cache, _rt, _dir) = build_core(&[("foo.txt", b"BASE-CONTENT\n")]);
        let foo = ino_of(&core, "foo.txt").unwrap();
        let attr = core.truncate(foo, 4).expect("truncate");
        assert_eq!(attr.size, 4);
        assert_eq!(core.read_full(foo).unwrap().unwrap(), b"BASE");
    }
}
