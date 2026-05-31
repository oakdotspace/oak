//! FUSE filesystem implementation backing `oak mount`.
//!
//! Reads come from three sources, in order:
//!   1. Overlay file (writes the user has made in this session)
//!   2. Local SQLite blob cache
//!   3. Remote, fetched on demand and cached
//!
//! Writes always materialize the file into the overlay first — once written,
//! a file lives in `overlay/` until `oak commit` rolls it into a real
//! Oak commit on the virtual branch.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use oak_core::{path_in_any_prefix, FileMode, Hash, Manifest, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use tokio::runtime::Handle;

use super::state::{self, DirtyEntry, MountConfig, OverlayMeta};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INODE: u64 = 1;

/// A cheap fingerprint of `overlay-meta.json` on disk. The FUSE server is a
/// long-running process, but `oak commit` / `oak status` / `oak push` each run
/// as *separate* processes that mutate the same mount state. After an
/// out-of-band `oak commit` clears the overlay and advances the virtual
/// branch, the server would otherwise keep serving the pre-commit base blobs
/// it captured at mount time (the working tree appears to "revert to base"
/// while `oak status` reports clean). We detect that by stat-ing the overlay
/// metadata file before content reads and reconciling when this signature
/// changes out from under us.
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

/// What kind of node a given inode represents.
#[derive(Debug, Clone)]
enum NodeKind {
    Directory,
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

pub struct MountFs {
    /// Inode → Node, protected by a mutex so FUSE worker threads can mutate.
    inner: Arc<Mutex<Inner>>,
    /// Mount config (immutable for the session).
    cfg: MountConfig,
    /// Bearer token for the remote (looked up from credentials at mount time).
    token: Option<String>,
    /// Tokio runtime handle so we can call async fetch helpers from FUSE
    /// callbacks (which are sync).
    rt: Handle,
    /// Overlay directory.
    overlay_dir: PathBuf,
    /// Mount state directory (where overlay-meta.json lives).
    state_dir: PathBuf,
    /// Local SQLite cache wrapped in Arc so we can hand it to async tasks.
    cache: Arc<SqliteRepository>,
    /// Stable mtime for clean (un-dirtied) files and directories — the
    /// timestamp of the base commit we mounted. Returning a *fresh*
    /// `SystemTime::now()` on every `getattr`/`lookup` makes editors like
    /// vim think the file changed between open and write, triggering the
    /// "WARNING: file has changed since reading it" prompt.
    base_mtime: SystemTime,
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
    /// Virtual-branch head the in-memory tree currently reflects. Advances
    /// only via an out-of-band `oak commit`; when we notice it moved we
    /// rebuild base blobs/sizes from the new head manifest.
    last_head: Option<Hash>,
}

impl MountFs {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: MountConfig,
        cache: Arc<SqliteRepository>,
        manifest: &Manifest,
        sizes: &HashMap<String, u64>,
        token: Option<String>,
        rt: Handle,
        state_dir: PathBuf,
        prefixes: &[String],
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
                kind: NodeKind::Directory,
                children: HashMap::new(),
                parent: ROOT_INODE,
            },
        );
        let mut next_ino = ROOT_INODE + 1;

        // Build the tree from the manifest, skipping paths the user has
        // deleted in this mount session.
        let deleted: std::collections::HashSet<&str> =
            overlay.deletions.iter().map(|s| s.as_str()).collect();
        // Apply renames (old path → new path) so the path we display is
        // the post-rename path. This is a simple flat rename; nested
        // renames within a renamed dir aren't tracked here.
        let renames = &overlay.renames;

        for entry in &manifest.entries {
            if deleted.contains(entry.path.as_str()) {
                continue;
            }
            let display_path = renames
                .get(&entry.path)
                .cloned()
                .unwrap_or_else(|| entry.path.clone());
            // Apply the project-scope filter (if any) before inserting
            // into the FUSE tree. We test the post-rename display path so
            // a rename out of scope hides the file, and a rename into
            // scope exposes it.
            if !prefixes.is_empty() && !path_in_any_prefix(prefixes, &display_path) {
                continue;
            }
            let size = sizes.get(entry.blob_hash.as_str()).copied().unwrap_or(0);
            let kind = NodeKind::File {
                base_blob: Some(entry.blob_hash.clone()),
                size,
                mode: entry.mode,
            };
            insert_path(&mut nodes, &mut next_ino, &display_path, kind);
        }

        // Layer in dirty files: any path in the overlay that isn't already
        // in the tree (e.g. a file the user just created). For files that
        // already exist we update their size.
        for (path, dirty) in &overlay.dirty {
            let real_size = std::fs::metadata(overlay_dir.join(&dirty.overlay_file))
                .map(|m| m.len())
                .unwrap_or(0);
            let mode = match dirty.mode.as_str() {
                "executable" => FileMode::Executable,
                "symlink" => FileMode::Symlink,
                _ => FileMode::Regular,
            };
            // If the path already exists, just refresh its size + mode.
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
                insert_path(
                    &mut nodes,
                    &mut next_ino,
                    path,
                    NodeKind::File {
                        base_blob: None,
                        size: real_size,
                        mode,
                    },
                );
            }
        }

        // Record the state we're consistent with at mount time so a later
        // out-of-band commit (which clears the overlay and advances the
        // virtual branch) is detectable. The tree was just built from the
        // virtual branch's current head manifest.
        let last_overlay_sig = overlay_sig(&state_dir);
        let last_head = cache.get_branch_head(&cfg.virtual_branch).ok().flatten();

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
            base_mtime,
        })
    }

    /// Pick the mtime to report for a node. Dirty files use their overlay
    /// file's actual mtime (so stat-then-write tools see a stable value
    /// per disk write); everything else returns the head commit's
    /// timestamp.
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
}

/// Walk the tree, creating directory nodes as needed, and place a file or
/// directory at the given relative path. Used during initial tree build.
fn insert_path(nodes: &mut HashMap<u64, Node>, next_ino: &mut u64, path: &str, kind: NodeKind) {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    let mut parent_ino = ROOT_INODE;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        let name = OsString::from(*part);

        // If the child already exists, descend into it (or, if last, skip —
        // duplicate insert).
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
            NodeKind::Directory
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

fn epoch() -> SystemTime {
    UNIX_EPOCH
}

fn perm_for(mode: FileMode) -> u16 {
    match mode {
        FileMode::Executable => 0o755,
        FileMode::Symlink => 0o777,
        FileMode::Regular => 0o644,
    }
}

fn dir_attr(ino: u64, mtime: SystemTime) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: epoch(),
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn file_attr(ino: u64, size: u64, mode: FileMode, mtime: SystemTime) -> FileAttr {
    let kind = match mode {
        FileMode::Symlink => FileType::Symlink,
        _ => FileType::RegularFile,
    };
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: epoch(),
        kind,
        perm: perm_for(mode),
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl MountFs {
    /// Read the full content for an inode (overlay → cache → remote).
    /// Returns None for directories.
    fn read_full(&self, ino: u64) -> std::io::Result<Option<Vec<u8>>> {
        let (path, base_blob) = {
            let inner = self.inner.lock().unwrap();
            let node = inner
                .nodes
                .get(&ino)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            match &node.kind {
                NodeKind::Directory => return Ok(None),
                NodeKind::File { base_blob, .. } => (node.path.clone(), base_blob.clone()),
            }
        };

        // 1. Overlay
        let overlay_name = state::overlay_filename_for(&path);
        let overlay_path = self.overlay_dir.join(&overlay_name);
        if overlay_path.exists() {
            return std::fs::read(&overlay_path).map(Some);
        }

        // 2. Cache → 3. Remote (via ensure_blobs_local)
        let Some(blob_hash) = base_blob else {
            // No base blob and no overlay file — empty new file.
            return Ok(Some(Vec::new()));
        };

        let blob = self.fetch_blob(&blob_hash).map_err(io_err)?;
        Ok(Some(blob))
    }

    /// Synchronously fetch a blob from the cache, falling back to the remote.
    fn fetch_blob(&self, hash: &Hash) -> Result<Vec<u8>> {
        if let Some(blob) = self.cache.get_blob(hash)? {
            return Ok(blob.content);
        }
        let cache = self.cache.clone();
        let remote = self.cfg.remote_url.clone();
        let owner = self.cfg.owner.clone();
        let repo = self.cfg.repo.clone();
        let token = self.token.clone();
        let h = hash.clone();
        self.rt.block_on(async move {
            super::super::blob_fetch::ensure_blobs_local(
                cache.as_ref(),
                &remote,
                &owner,
                &repo,
                token.as_deref(),
                std::slice::from_ref(&h),
            )
            .await
        })?;
        let blob = self
            .cache
            .get_blob(hash)?
            .ok_or_else(|| OakError::Server(format!("blob {} unavailable after fetch", hash)))?;
        Ok(blob.content)
    }

    /// Materialize a file into the overlay if it isn't already there. The
    /// file's current content (from cache/remote) is copied first so writes
    /// behave like opening an existing file.
    fn materialize_to_overlay(&self, ino: u64) -> std::io::Result<PathBuf> {
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
        // Record in overlay-meta.
        let mode = {
            let inner = self.inner.lock().unwrap();
            match inner.nodes.get(&ino).map(|n| n.kind.clone()) {
                Some(NodeKind::File { mode, .. }) => mode,
                _ => FileMode::Regular,
            }
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.overlay.dirty.insert(
                path.clone(),
                DirtyEntry {
                    overlay_file: overlay_name,
                    mode: match mode {
                        FileMode::Executable => "executable".into(),
                        FileMode::Symlink => "symlink".into(),
                        FileMode::Regular => "regular".into(),
                    },
                    in_place: false,
                },
            );
        }
        self.persist_overlay().map_err(io_err)?;
        Ok(overlay_path)
    }

    fn persist_overlay(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        state::save_overlay_meta(&self.state_dir, &inner.overlay)?;
        // Remember the signature of what we just wrote so this write doesn't
        // later look like an external change and trigger a needless reconcile.
        inner.last_overlay_sig = overlay_sig(&self.state_dir);
        Ok(())
    }

    /// Cheaply detect, and reconcile with, an out-of-band change to the mount
    /// state — the usual cause being `oak commit` running in another process,
    /// which clears `overlay-meta.json`, deletes the overlay files, and
    /// advances the virtual branch. Without this, the next read of a
    /// just-committed file finds no overlay file and falls back to the stale
    /// `base_blob` captured at mount time, silently serving pre-commit
    /// content (the "working tree reverts to base after commit" bug).
    ///
    /// When the on-disk overlay signature is unchanged this does a single
    /// `stat` and returns. fuser serializes filesystem callbacks through
    /// `&mut self`, so this never races another handler.
    fn reconcile_if_external_change(&self) {
        let sig = overlay_sig(&self.state_dir);
        let mut inner = self.inner.lock().unwrap();
        if inner.last_overlay_sig == sig {
            return;
        }
        if let Err(e) = self.reconcile_locked(&mut inner) {
            tracing::warn!(?e, "mount: reconcile after external overlay change failed");
        }
        // Record the signature we acted on regardless of outcome; a failed
        // reconcile shouldn't busy-loop re-reading on every subsequent op.
        inner.last_overlay_sig = sig;
    }

    /// Rebuild in-memory base blobs/sizes from the current virtual-branch head
    /// manifest. Called with `inner` already locked.
    fn reconcile_locked(&self, inner: &mut Inner) -> Result<()> {
        // Mirror whatever the overlay metadata says now (a commit cleared it).
        inner.overlay = state::load_overlay_meta(&self.state_dir)?;

        let head = self.cache.get_branch_head(&self.cfg.virtual_branch)?;
        if head == inner.last_head {
            // Overlay changed but the branch didn't advance (e.g. a no-op
            // commit, or someone hand-edited the meta). Nothing to rebuild.
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

        // Snapshot the overlay bits we need so we can mutate `nodes` freely.
        let renames = inner.overlay.renames.clone();
        let deleted: HashSet<String> = inner.overlay.deletions.iter().cloned().collect();
        let dirty: HashSet<String> = inner.overlay.dirty.keys().cloned().collect();

        // Post-commit desired tree: manifest paths (minus user deletions),
        // under the same rename overlay and project-scope filter the initial
        // build applied, mapped to their committed blob + mode.
        let prefixes = &self.cfg.path_prefixes;
        let mut want: HashMap<String, (Hash, FileMode)> = HashMap::new();
        for entry in &manifest.entries {
            if deleted.contains(&entry.path) {
                continue;
            }
            let display = renames
                .get(&entry.path)
                .cloned()
                .unwrap_or_else(|| entry.path.clone());
            if !prefixes.is_empty() && !path_in_any_prefix(prefixes, &display) {
                continue;
            }
            want.insert(display, (entry.blob_hash.clone(), entry.mode));
        }

        // Update changed nodes, add newly-committed paths. Skip anything the
        // user currently has dirty — its overlay file is authoritative and
        // already reads fresh.
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
                    let size = self.blob_size(blob).unwrap_or(0);
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
                    let size = self.blob_size(blob).unwrap_or(0);
                    let kind = NodeKind::File {
                        base_blob: Some(blob.clone()),
                        size,
                        mode: *mode,
                    };
                    insert_path(&mut inner.nodes, &mut inner.next_ino, path, kind);
                }
            }
        }

        // Drop file nodes the commit removed (no longer in the manifest) and
        // that the user isn't keeping dirty.
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

fn io_err(e: OakError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl Filesystem for MountFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.reconcile_if_external_change();
        let inner = self.inner.lock().unwrap();
        let Some(parent_node) = inner.nodes.get(&parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(&child_ino) = parent_node.children.get(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(child) = inner.nodes.get(&child_ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let mtime = self.mtime_for_node(child);
        let attr = match &child.kind {
            NodeKind::Directory => dir_attr(child.ino, mtime),
            NodeKind::File { size, mode, .. } => file_attr(child.ino, *size, *mode, mtime),
        };
        reply.entry(&TTL, &attr, 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        self.reconcile_if_external_change();
        let inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let mtime = self.mtime_for_node(node);
        let attr = match &node.kind {
            NodeKind::Directory => dir_attr(node.ino, mtime),
            NodeKind::File { size, mode, .. } => file_attr(node.ino, *size, *mode, mtime),
        };
        reply.attr(&TTL, &attr);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        self.reconcile_if_external_change();
        let inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !matches!(node.kind, NodeKind::Directory) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let mut entries: Vec<(u64, FileType, OsString)> =
            Vec::with_capacity(2 + node.children.len());
        entries.push((node.ino, FileType::Directory, OsString::from(".")));
        entries.push((node.parent, FileType::Directory, OsString::from("..")));
        for (name, &child_ino) in &node.children {
            let kind = match inner.nodes.get(&child_ino).map(|n| &n.kind) {
                Some(NodeKind::Directory) => FileType::Directory,
                Some(NodeKind::File {
                    mode: FileMode::Symlink,
                    ..
                }) => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            entries.push((child_ino, kind, name.clone()));
        }

        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // i+1 because offset is the *next* offset to use, not the current.
            if reply.add(ino, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // We don't track per-fh state; just hand back fh=0.
        //
        // FOPEN_DIRECT_IO: bypass the kernel page cache so every read()/write()
        // syscall maps 1:1 onto a FUSE read/write op with the exact offset and
        // bytes. Without it the kernel buffers writes in page-sized chunks and
        // flushes them back against a possibly-stale i_size, zero-padding
        // partial / hole pages on writeback — which corrupted overlay files
        // with scattered NUL bytes. Our read/write handlers operate on whole
        // files anyway, so we gain nothing from page-cache buffering.
        reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        self.reconcile_if_external_change();
        match self.read_full(ino) {
            Ok(Some(content)) => {
                let start = offset.max(0) as usize;
                if start >= content.len() {
                    reply.data(&[]);
                    return;
                }
                let end = (start + size as usize).min(content.len());
                reply.data(&content[start..end]);
            }
            Ok(None) => reply.error(libc::EISDIR),
            Err(e) => {
                tracing::warn!(?e, ino, "mount read failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let overlay_path = match self.materialize_to_overlay(ino) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(?e, ino, "materialize failed");
                reply.error(libc::EIO);
                return;
            }
        };

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
            // Update the cached size on the inode.
            let mut inner = self.inner.lock().unwrap();
            if let Some(node) = inner.nodes.get_mut(&ino) {
                if let NodeKind::File { size, .. } = &mut node.kind {
                    *size = new_size;
                }
            }
            Ok(data.len() as u32)
        })();

        match result {
            Ok(n) => reply.written(n),
            Err(e) => {
                tracing::warn!(?e, ino, "mount write failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let exec = (mode & 0o111) != 0;
        let file_mode = if exec {
            FileMode::Executable
        } else {
            FileMode::Regular
        };
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let new_path = {
            let inner = self.inner.lock().unwrap();
            let Some(parent_node) = inner.nodes.get(&parent) else {
                reply.error(libc::ENOENT);
                return;
            };
            if !matches!(parent_node.kind, NodeKind::Directory) {
                reply.error(libc::ENOTDIR);
                return;
            }
            if parent_node.children.contains_key(name) {
                reply.error(libc::EEXIST);
                return;
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
            reply.error(libc::EIO);
            return;
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
            inner.overlay.dirty.insert(
                new_path.clone(),
                DirtyEntry {
                    overlay_file: overlay_name,
                    mode: if exec {
                        "executable".into()
                    } else {
                        "regular".into()
                    },
                    in_place: false,
                },
            );
            ino
        };
        if let Err(e) = self.persist_overlay() {
            tracing::warn!(?e, "create: persist overlay-meta");
        }

        let attr = {
            let inner = self.inner.lock().unwrap();
            match inner.nodes.get(&new_ino) {
                Some(n) => file_attr(new_ino, 0, file_mode, self.mtime_for_node(n)),
                None => file_attr(new_ino, 0, file_mode, self.base_mtime),
            }
        };
        // FOPEN_DIRECT_IO on the freshly-created handle for the same reason as
        // `open` — keep file data off the kernel page cache to avoid
        // writeback zero-padding corrupting the overlay.
        reply.created(&TTL, &attr, 0, 0, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let Some(parent_node) = inner.nodes.get(&parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !matches!(parent_node.kind, NodeKind::Directory) {
            reply.error(libc::ENOTDIR);
            return;
        }
        if parent_node.children.contains_key(name) {
            reply.error(libc::EEXIST);
            return;
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
                kind: NodeKind::Directory,
                children: HashMap::new(),
                parent,
            },
        );
        if let Some(p) = inner.nodes.get_mut(&parent) {
            p.children.insert(name.to_owned(), ino);
        }
        // Directories aren't first-class in the manifest model — they'll
        // appear implicitly when files inside them are committed. So we
        // don't record this in overlay-meta; an empty mkdir without a file
        // inside it has no commit-time effect.
        let attr = dir_attr(ino, self.base_mtime);
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let path = {
            let mut inner = self.inner.lock().unwrap();
            let Some(parent_node) = inner.nodes.get(&parent) else {
                reply.error(libc::ENOENT);
                return;
            };
            let Some(&child_ino) = parent_node.children.get(name) else {
                reply.error(libc::ENOENT);
                return;
            };
            let Some(child) = inner.nodes.get(&child_ino).cloned() else {
                reply.error(libc::ENOENT);
                return;
            };
            if matches!(child.kind, NodeKind::Directory) {
                reply.error(libc::EISDIR);
                return;
            }

            // Remove from parent + nodes table.
            if let Some(p) = inner.nodes.get_mut(&parent) {
                p.children.remove(name);
            }
            inner.nodes.remove(&child_ino);

            // Bookkeeping: if the file existed in the base manifest, record
            // a deletion. If it was a dirty overlay file, drop it.
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
        // Remove the overlay file if any.
        let overlay_path = self.overlay_dir.join(state::overlay_filename_for(&path));
        if overlay_path.exists() {
            let _ = std::fs::remove_file(&overlay_path);
        }
        if let Err(e) = self.persist_overlay() {
            tracing::warn!(?e, "unlink: persist overlay-meta");
            reply.error(libc::EIO);
            return;
        }
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let mut inner = self.inner.lock().unwrap();
        let Some(parent_node) = inner.nodes.get(&parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(&child_ino) = parent_node.children.get(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(child) = inner.nodes.get(&child_ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !matches!(child.kind, NodeKind::Directory) {
            reply.error(libc::ENOTDIR);
            return;
        }
        if !child.children.is_empty() {
            reply.error(libc::ENOTEMPTY);
            return;
        }
        inner.nodes.remove(&child_ino);
        if let Some(p) = inner.nodes.get_mut(&parent) {
            p.children.remove(name);
        }
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let result: std::io::Result<()> = (|| {
            let (old_path, new_path, child_ino) = {
                let mut inner = self.inner.lock().unwrap();
                let parent_node = inner
                    .nodes
                    .get(&parent)
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
                let &child_ino = parent_node
                    .children
                    .get(name)
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
                let child = inner
                    .nodes
                    .get(&child_ino)
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?
                    .clone();
                let new_parent_node = inner
                    .nodes
                    .get(&newparent)
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
                let new_path = if new_parent_node.path.is_empty() {
                    newname.to_string_lossy().into_owned()
                } else {
                    format!("{}/{}", new_parent_node.path, newname.to_string_lossy())
                };
                let old_path = child.path.clone();

                // Update parent maps.
                if let Some(p) = inner.nodes.get_mut(&parent) {
                    p.children.remove(name);
                }
                if let Some(np) = inner.nodes.get_mut(&newparent) {
                    np.children.insert(newname.to_owned(), child_ino);
                }
                // Update node path + parent.
                if let Some(node) = inner.nodes.get_mut(&child_ino) {
                    node.path = new_path.clone();
                    node.parent = newparent;
                }

                // Bookkeeping.
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
            self.persist_overlay().map_err(io_err)?;
            tracing::debug!(%old_path, %new_path, ino = child_ino, "rename");
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => {
                tracing::warn!(?e, "rename failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            let overlay_path = match self.materialize_to_overlay(ino) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(?e, ino, "setattr: materialize failed");
                    reply.error(libc::EIO);
                    return;
                }
            };
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(&overlay_path)
                .and_then(|f| f.set_len(new_size))
            {
                tracing::warn!(?e, "setattr: truncate failed");
                reply.error(libc::EIO);
                return;
            }
            let mut inner = self.inner.lock().unwrap();
            if let Some(node) = inner.nodes.get_mut(&ino) {
                if let NodeKind::File { size, .. } = &mut node.kind {
                    *size = new_size;
                }
            }
        }

        let inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let mtime = self.mtime_for_node(node);
        let attr = match &node.kind {
            NodeKind::Directory => dir_attr(node.ino, mtime),
            NodeKind::File { size, mode, .. } => file_attr(node.ino, *size, *mode, mtime),
        };
        reply.attr(&TTL, &attr);
    }
}

/// Mount the FUSE filesystem on the given path and run until interrupted.
/// Blocks the caller's thread.
pub fn mount_fs(mount_point: &Path, fs: MountFs) -> Result<()> {
    #[allow(unused_mut)]
    let mut opts = vec![
        fuser::MountOption::FSName(format!("oak:{}", fs.cfg.id.clone())),
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::DefaultPermissions,
    ];

    // macOS Finder, Spotlight, and various preserve-attribute paths spill
    // extended attributes into AppleDouble (`._<name>`) sidecars whenever
    // they write to a filesystem that doesn't natively support xattrs —
    // which FUSE mounts qualify for. macFUSE recognizes these two flags
    // and rejects the writes at the kernel level, so we never see the
    // sidecars in the overlay. Source-code mounts have no use for Finder
    // labels or resource forks; suppressing is the right default.
    #[cfg(target_os = "macos")]
    {
        opts.push(fuser::MountOption::CUSTOM("noappledouble".into()));
        opts.push(fuser::MountOption::CUSTOM("noapplexattr".into()));
    }

    fuser::mount2(fs, mount_point, &opts).map_err(OakError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fresh `nodes` map containing only the root inode.
    fn fresh() -> (HashMap<u64, Node>, u64) {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                ino: ROOT_INODE,
                path: String::new(),
                kind: NodeKind::Directory,
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

        // Root → src → utils → io.rs (4 nodes total)
        assert_eq!(nodes.len(), 4);

        let src_ino = lookup_path(&nodes, "src").expect("src dir");
        let utils_ino = lookup_path(&nodes, "src/utils").expect("utils dir");
        let leaf_ino = lookup_path(&nodes, "src/utils/io.rs").expect("file");

        assert!(matches!(nodes[&src_ino].kind, NodeKind::Directory));
        assert!(matches!(nodes[&utils_ino].kind, NodeKind::Directory));
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

    use oak_core::{Blob, FileChange, ManifestEntry};
    use tempfile::TempDir;

    /// Build a `MountFs` over a temp SQLite cache whose virtual branch `vb`
    /// has a single commit containing `files`. Returns the fs plus the live
    /// cache handle (to simulate out-of-band commits), the tokio runtime
    /// (kept alive so the stored `Handle` stays valid), and the temp dir.
    fn build_fs(
        files: &[(&str, &[u8])],
    ) -> (
        MountFs,
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
            team: None,
            project: None,
            path_prefixes: Vec::new(),
        };
        let mut sizes = HashMap::new();
        for e in &manifest.entries {
            let b = cache.get_blob(&e.blob_hash).unwrap().unwrap();
            sizes.insert(e.blob_hash.as_str().to_string(), b.size);
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fs = MountFs::new(
            cfg,
            cache.clone(),
            &manifest,
            &sizes,
            None,
            rt.handle().clone(),
            state_dir,
            &[],
            SystemTime::now(),
        )
        .unwrap();
        (fs, cache, rt, dir)
    }

    fn head_of(cache: &SqliteRepository) -> Hash {
        cache.get_branch_head("vb").unwrap().unwrap()
    }

    /// Store blobs + a manifest + a commit on `vb` (advancing past `parent`).
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

    fn ino_of(fs: &MountFs, path: &str) -> Option<u64> {
        let inner = fs.inner.lock().unwrap();
        lookup_path(&inner.nodes, path)
    }

    fn node_size(fs: &MountFs, ino: u64) -> u64 {
        let inner = fs.inner.lock().unwrap();
        match &inner.nodes.get(&ino).unwrap().kind {
            NodeKind::File { size, .. } => *size,
            _ => panic!("not a file"),
        }
    }

    // The core regression for the "post-commit working-tree reverts to base"
    // bug: after a separate process commits, the long-running server must
    // serve the committed content (not the stale mount-time base blob), report
    // the new size, surface added files, and drop deleted ones.
    #[test]
    fn reconcile_follows_out_of_band_commit() {
        let (fs, cache, _rt, dir) = build_fs(&[("foo.txt", b"BASE\n"), ("gone.txt", b"X\n")]);
        let state_dir = dir.path().to_path_buf();

        let foo = ino_of(&fs, "foo.txt").expect("foo present");
        assert_eq!(fs.read_full(foo).unwrap().unwrap(), b"BASE\n");
        assert!(ino_of(&fs, "gone.txt").is_some());

        const NEW: &[u8] = b"NEW CONTENT, LONGER THAN BASE\n";
        external_commit(
            &cache,
            &state_dir,
            &[("foo.txt", NEW), ("added.txt", b"added\n")],
        );

        // Before the fix this read found no overlay file and fell back to the
        // stale base blob, reverting to "BASE\n". Now it reconciles.
        fs.reconcile_if_external_change();

        assert_eq!(
            fs.read_full(foo).unwrap().unwrap(),
            NEW,
            "read must follow committed content, not revert to base"
        );
        assert_eq!(
            node_size(&fs, foo),
            NEW.len() as u64,
            "reported size must match committed content (avoids truncated reads under direct_io)"
        );

        let added = ino_of(&fs, "added.txt").expect("added.txt should appear");
        assert_eq!(fs.read_full(added).unwrap().unwrap(), b"added\n");
        assert!(
            ino_of(&fs, "gone.txt").is_none(),
            "committed deletion should drop the node"
        );
    }

    // A dirty (uncommitted) overlay file must win over a reconcile — the user's
    // in-flight edit is authoritative even if the branch advanced underneath.
    #[test]
    fn reconcile_preserves_dirty_overlay() {
        let (fs, cache, _rt, dir) = build_fs(&[("foo.txt", b"BASE\n")]);
        let state_dir = dir.path().to_path_buf();

        // User edits foo.txt in the mount (materializes an overlay file).
        let overlay_dir = state::overlay_dir(&state_dir);
        std::fs::create_dir_all(&overlay_dir).unwrap();
        std::fs::write(
            overlay_dir.join(state::overlay_filename_for("foo.txt")),
            b"MY EDIT\n",
        )
        .unwrap();
        {
            let mut inner = fs.inner.lock().unwrap();
            inner.overlay.dirty.insert(
                "foo.txt".into(),
                DirtyEntry {
                    overlay_file: state::overlay_filename_for("foo.txt"),
                    mode: "regular".into(),
                    in_place: false,
                },
            );
        }
        fs.persist_overlay().unwrap();

        // A separate process commits something else (foo.txt untouched there).
        external_commit(
            &cache,
            &state_dir,
            &[("foo.txt", b"BASE\n"), ("other.txt", b"o\n")],
        );
        // ...but our overlay-meta still lists foo.txt dirty (re-add after the
        // external clear, mimicking that our edit is still in flight).
        {
            let mut inner = fs.inner.lock().unwrap();
            inner.overlay.dirty.insert(
                "foo.txt".into(),
                DirtyEntry {
                    overlay_file: state::overlay_filename_for("foo.txt"),
                    mode: "regular".into(),
                    in_place: false,
                },
            );
        }
        fs.persist_overlay().unwrap();
        fs.reconcile_if_external_change();

        let foo = ino_of(&fs, "foo.txt").unwrap();
        assert_eq!(
            fs.read_full(foo).unwrap().unwrap(),
            b"MY EDIT\n",
            "dirty overlay content must survive an out-of-band reconcile"
        );
    }

    #[test]
    fn reconcile_is_noop_without_external_change() {
        let (fs, _cache, _rt, _dir) = build_fs(&[("foo.txt", b"BASE\n")]);
        let foo = ino_of(&fs, "foo.txt").unwrap();
        fs.reconcile_if_external_change();
        fs.reconcile_if_external_change();
        assert_eq!(fs.read_full(foo).unwrap().unwrap(), b"BASE\n");
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
}
