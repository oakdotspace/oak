use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dialoguer::Confirm;
use oak_core::{
    detect_renames_by_content, BlobSource, BranchStatus, ChangeType, FileChange, FileMode, Hash,
    IgnorePatterns, Manifest, ManifestEntry, MetadataKey, OakError, RenameConfig, Result,
};
use oak_core::{Repository, StatCacheEntry};
use rayon::prelude::*;
use serde::Serialize;

use super::hooks;
use super::push;

use crate::output;

/// Get the author name from environment or default
pub fn get_author() -> String {
    std::env::var("OAK_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Scan the working directory and create manifest entries.
///
/// Each file's content is hashed and stored via `repo.put_blob`, so the resulting
/// `ManifestEntry::blob_hash` is whatever the backend considers canonical (BLAKE3
/// for sqlite, git OID for the git backend).
///
/// Scan the working tree, trusting the working-tree stat cache for files whose
/// `(mtime, ctime, size)` are unchanged. This is the fast path used by
/// ordinary `oak commit` and `oak diff`; status uses a non-persisting scan
/// below.
pub fn scan_working_dir(
    root: &Path,
    work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
) -> Result<Vec<ManifestEntry>> {
    Ok(scan_working_dir_inner(root, work_tree, repo, ignore, true, true)?.entries)
}

/// Scan WITHOUT trusting the stat cache: every file is re-read and re-hashed,
/// and the cache is rebuilt from what's actually on disk. Used by the
/// conflict-resolution scans (`oak pull --continue` / `oak merge --continue`),
/// which run rarely but at high stakes. Re-hashing there guarantees the
/// recorded manifest reflects on-disk content and can never echo a stale
/// cache row left behind by a branch-switch/sync that materialized another
/// branch's version of a path — the foreign-blob data-integrity bug that the
/// materializer cache-refresh (`refresh_stat_cache_after_materialize`) also
/// guards against from the other direction.
pub fn scan_working_dir_no_cache(
    root: &Path,
    work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
) -> Result<Vec<ManifestEntry>> {
    Ok(scan_working_dir_inner(root, work_tree, repo, ignore, false, true)?.entries)
}

struct WorkingDirScan {
    entries: Vec<ManifestEntry>,
    /// Content read for cache misses when the scan intentionally did not store
    /// those blobs. Status needs this only for rename-with-edit detection.
    ephemeral_blobs: HashMap<Hash, Vec<u8>>,
}

fn scan_working_dir_for_changes(
    root: &Path,
    work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
) -> Result<WorkingDirScan> {
    scan_working_dir_inner(root, work_tree, repo, ignore, true, false)
}

/// Shared implementation. `use_cache` gates only whether an unchanged
/// `(mtime, ctime, size)` may *reuse* a cached blob hash. `store_misses` gates
/// whether freshly read files are persisted and written back to the stat cache.
/// Vanished paths are still pruned from the cache.
fn scan_working_dir_inner(
    root: &Path,
    _work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
    use_cache: bool,
    store_misses: bool,
) -> Result<WorkingDirScan> {
    let activity = output::activity("Scanning working tree...");

    // One file the walk found: enough metadata to consult the stat cache
    // without touching contents. `mtime_ns` is `None` when the platform can't
    // report a modification time, in which case the file is always re-hashed
    // and never cached. `ctime_ns` (inode change time) is the forgery guard on
    // top of `(mtime, size)` — see `StatCacheEntry`; it's 0 where the platform
    // has no POSIX ctime.
    struct WalkedFile {
        rel_path: String,
        abs_path: PathBuf,
        size: i64,
        mtime_ns: Option<i64>,
        ctime_ns: i64,
        mode: FileMode,
    }

    // Phase 1: walk the directory tree (no file reads — it only stats
    // entries). Sibling directories are independent, so the recursion fans
    // out over rayon: on wide trees the per-entry stat + ignore-match cost is
    // spread across cores instead of paid serially. Results are concatenated
    // in task-completion order, which is fine — every consumer of the file
    // list is order-insensitive (`Manifest::new` sorts by path, and the diff
    // sorts its changes before printing).
    fn walk_dir(dir: &Path, root: &Path, ignore: &IgnorePatterns) -> Result<Vec<WalkedFile>> {
        fn walked_file(
            path: PathBuf,
            relative_path: &Path,
            file_type: fs::FileType,
        ) -> Result<Option<WalkedFile>> {
            if file_type.is_symlink() {
                // Record the symlink itself; its content is the target path,
                // read in phase 2 via `readlink` rather than by following it.
                // mtime is left unset so symlinks are never served from / written
                // to the stat cache — the materializer doesn't cache them either,
                // so the two stay consistent and symlinks are always re-read.
                Ok(Some(WalkedFile {
                    rel_path: relative_path.to_string_lossy().replace('\\', "/"),
                    abs_path: path,
                    size: 0,
                    mtime_ns: None,
                    ctime_ns: 0,
                    mode: FileMode::Symlink,
                }))
            } else if file_type.is_file() {
                let md = fs::metadata(&path)?;
                // Stat for the cache: size + mtime + ctime. ctime is the
                // forgery guard (bumps on any content write or mtime reset and
                // can't be moved backwards from userspace, so it catches an
                // edit that forges a matching `(mtime, size)`). Shared with the
                // materializer cache-refresh path via `file_stat_for_cache` so
                // the two can't drift.
                let (size, mtime_ns, ctime_ns) = file_stat_for_cache(&md);

                // Determine file mode
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    if md.permissions().mode() & 0o111 != 0 {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    }
                };

                #[cfg(not(unix))]
                let mode = FileMode::Regular;

                Ok(Some(WalkedFile {
                    // Forward-slashes are oak's canonical manifest separator.
                    // On Windows `to_string_lossy()` produces `\`, which would
                    // make `oak status` after a fresh clone report every file
                    // as renamed (`foo/bar` -> `foo\bar`) and break Linux ↔
                    // Windows interop on commits authored from Windows.
                    rel_path: relative_path.to_string_lossy().replace('\\', "/"),
                    abs_path: path,
                    size,
                    mtime_ns,
                    ctime_ns,
                    mode,
                }))
            } else {
                Ok(None)
            }
        }

        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative_path = path.strip_prefix(root).unwrap();

            // Classify without following symlinks: `DirEntry::file_type`
            // lstat's, so a symlink reports as a symlink even when it points at
            // a directory. `is_real_dir` is the only thing we descend into;
            // symlinks are recorded as entries in their own right.
            let file_type = entry.file_type()?;
            let is_real_dir = file_type.is_dir();

            if is_real_dir && relative_path == Path::new(".oak") {
                let attributes = path.join("attributes");
                match fs::symlink_metadata(&attributes) {
                    Ok(md) => {
                        let rel = Path::new(".oak/attributes");
                        if !ignore.is_ignored(rel, false) {
                            oak_core::validate_tree_path(
                                &rel.to_string_lossy().replace('\\', "/"),
                            )?;
                            if let Some(file) = walked_file(attributes, rel, md.file_type())? {
                                files.push(file);
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                continue;
            }

            if is_real_dir && relative_path.file_name().is_some_and(|name| name == ".oak") {
                continue;
            }

            // Skip ignored files (a symlink is never the directory it targets).
            if ignore.is_ignored(relative_path, is_real_dir) {
                continue;
            }

            // Reject names oak's canonical tree format can't represent (tab,
            // newline, NUL — all legal POSIX filename bytes) before anything
            // is hashed or stored, so the user gets one clear error naming
            // the file instead of corruption deep in tree serialization.
            // Checked per directory entry, so a hostile directory name is
            // caught here too, before descending into it.
            oak_core::validate_tree_path(&relative_path.to_string_lossy().replace('\\', "/"))?;

            if is_real_dir {
                subdirs.push(path);
            } else if let Some(file) = walked_file(path.clone(), relative_path, file_type)? {
                files.push(file);
            }
        }
        let nested: Vec<Vec<WalkedFile>> = subdirs
            .par_iter()
            .map(|d| walk_dir(d, root, ignore))
            .collect::<Result<_>>()?;
        files.extend(nested.into_iter().flatten());
        Ok(files)
    }

    let files = walk_dir(root, root, ignore)?;

    // Load the working-tree stat cache once. A file whose (mtime, ctime, size)
    // all match a cached row reuses that row's blob hash — no read, no re-hash.
    // This is what makes `oak status` cheap on huge trees where almost nothing
    // changed; ctime is what keeps it from being fooled by a forged (mtime,
    // size) (see `StatCacheEntry`).
    activity.set_message("Checking stat cache...");
    let cache = repo.load_stat_cache()?;

    // Phase 2: resolve each file to a manifest entry. Cache hits are pure
    // lookups; misses read + hash + store the blob, all through one
    // `put_blob_sources` batch: the backend reads, hashes, and encodes the
    // files fused per file in parallel and streams them into a single
    // transaction — instead of one autocommit (WAL append + sync) per changed
    // file — while bounding in-flight content by the size hints. Each miss
    // also emits a fresh cache row to persist.
    type ScanItem = (ManifestEntry, Option<(String, StatCacheEntry)>);
    let mut items: Vec<Option<ScanItem>> = Vec::with_capacity(files.len());
    let mut misses: Vec<usize> = Vec::new();
    // Fast path: when every current path already has a cache row and the file
    // count still matches the cache, there cannot be any vanished cached path
    // to prune. This lets steady-state `oak status` avoid rebuilding a `seen`
    // set and re-checking every cache key on every run.
    let mut may_have_removed_paths = files.len() != cache.len();
    for (idx, f) in files.iter().enumerate() {
        let cached = cache.get(&f.rel_path);
        // Trust a cached hash only in cache mode, and only when mtime,
        // ctime, AND size all match. ctime is the forgery guard: an edit
        // that reuses an old mtime at the same size still bumps ctime, so
        // it misses here and gets re-hashed below (see `StatCacheEntry`).
        // `oak pull/merge --continue` pass `use_cache = false` to force a
        // re-hash regardless (see `scan_working_dir_no_cache`).
        if use_cache {
            if let (Some(mtime_ns), Some(prev)) = (f.mtime_ns, cached) {
                if prev.mtime_ns == mtime_ns && prev.ctime_ns == f.ctime_ns && prev.size == f.size {
                    items.push(Some((
                        ManifestEntry {
                            path: f.rel_path.clone(),
                            blob_hash: prev.blob_hash.clone(),
                            mode: f.mode,
                        },
                        None,
                    )));
                    continue;
                }
            }
        }
        if cached.is_none() {
            may_have_removed_paths = true;
        }
        items.push(None);
        misses.push(idx);
    }

    // Hand the backend deferred readers, not pre-read contents: the backend
    // fuses read + hash + encode per file and streams the results straight
    // into its store, so one file's read overlaps another's hashing AND the
    // already-finished files' inserts (pre-partitioned batches had barriers at
    // every boundary, which serialized snapshots of a few large files). The
    // size hint — the walk's stat size — is what lets the backend bound its
    // in-flight content without those batches. A symlink's content is its
    // target path (read via `readlink`), never the bytes it points at — that's
    // what makes it round-trip with the symlink entry the git import / commit
    // recorded.
    let sources: Vec<BlobSource> = misses
        .iter()
        .map(|&i| {
            let f = &files[i];
            let abs_path = f.abs_path.clone();
            let is_symlink = f.mode == FileMode::Symlink;
            BlobSource::new(Some(f.size.max(0) as u64), move || {
                if is_symlink {
                    symlink_target_bytes(&abs_path)
                } else {
                    Ok(fs::read(&abs_path)?)
                }
            })
        })
        .collect();
    // Commit stores misses through the backend so it gets canonical OIDs and
    // durable blobs. Status only needs hashes for classification, so it hashes
    // the read bytes in memory and keeps them available for rename scoring.
    if !misses.is_empty() {
        activity.set_message(format!("Hashing {} changed file(s)...", misses.len()));
    }
    let mut ephemeral_blobs = HashMap::new();
    let hashes: Vec<Hash> = if store_misses {
        repo.put_blob_sources(sources)?
    } else {
        sources
            .into_par_iter()
            .map(|source| -> Result<(Hash, Vec<u8>)> {
                let content = (source.read)()?;
                let hash = oak_core::hash_bytes(&content);
                Ok((hash, content))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|(hash, content)| {
                ephemeral_blobs.entry(hash.clone()).or_insert(content);
                hash
            })
            .collect()
    };
    for (&i, blob_hash) in misses.iter().zip(hashes) {
        let f = &files[i];
        let upsert = if store_misses {
            f.mtime_ns.map(|mtime_ns| {
                (
                    f.rel_path.clone(),
                    StatCacheEntry {
                        mtime_ns,
                        ctime_ns: f.ctime_ns,
                        size: f.size,
                        blob_hash: blob_hash.clone(),
                    },
                )
            })
        } else {
            None
        };
        items[i] = Some((
            ManifestEntry {
                path: f.rel_path.clone(),
                blob_hash,
                mode: f.mode,
            },
            upsert,
        ));
    }

    let mut entries = Vec::with_capacity(items.len());
    let mut upserts = Vec::new();
    for item in items {
        let (entry, upsert) = item.expect("every walked file resolves to a hit or a stored miss");
        entries.push(entry);
        if let Some(u) = upsert {
            upserts.push(u);
        }
    }

    // Phase 3: persist cache changes. Only rebuild the full path set when the
    // walk could have dropped a cached path; steady-state scans with no path
    // churn can write miss upserts directly (or skip the call entirely).
    activity.set_message("Updating stat cache...");
    if may_have_removed_paths {
        let seen: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let removed: Vec<String> = cache
            .keys()
            .filter(|k| !seen.contains(k.as_str()))
            .cloned()
            .collect();
        repo.update_stat_cache(&upserts, &removed)?;
    } else {
        repo.update_stat_cache(&upserts, &[])?;
    }

    Ok(WorkingDirScan {
        entries,
        ephemeral_blobs,
    })
}

/// Read a symlink's target as the bytes oak stores for it — the raw target
/// path, matching what git records as a symlink blob (and what
/// `git_clone::copy_tree_to_oak` imports). Never follows the link.
#[cfg(unix)]
fn symlink_target_bytes(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn symlink_target_bytes(path: &std::path::Path) -> Result<Vec<u8>> {
    Ok(fs::read_link(path)?
        .to_string_lossy()
        .into_owned()
        .into_bytes())
}

/// Extract the stat-cache triple `(size, mtime_ns, ctime_ns)` from a file's
/// metadata. `mtime_ns` is `None` when the platform can't report a
/// modification time (then the file is never cached and always re-hashed).
/// `ctime_ns` (inode change time) is the forgery guard on top of
/// `(mtime, size)`: it bumps on any content write or mtime reset and can't be
/// moved backwards from userspace; it's 0 where there's no POSIX ctime (e.g.
/// Windows), degrading the hit check to `(mtime, size)`. Shared by the
/// working-dir walk and by [`stat_cache_upsert`] so the two never drift.
pub(crate) fn file_stat_for_cache(md: &fs::Metadata) -> (i64, Option<i64>, i64) {
    let size = md.len() as i64;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    #[cfg(unix)]
    let ctime_ns = {
        use std::os::unix::fs::MetadataExt;
        md.ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(md.ctime_nsec())
    };
    #[cfg(not(unix))]
    let ctime_ns = 0i64;
    (size, mtime_ns, ctime_ns)
}

/// Build a stat-cache upsert for a file a materializer just wrote to
/// `abs_path` with known content hash `blob_hash`. Returns `None` — meaning
/// "don't cache this path, let the next scan re-hash it" — when the file can't
/// be stat'd or the platform reports no mtime.
///
/// Callers that materialize the working tree (branch switch, sync/merge reset,
/// conflict write-out, pull) MUST refresh the cache for what they wrote: the
/// `stat_cache` table is keyed by path only and shared across every branch in
/// the working dir, so a row left over from another branch's version of a path
/// would otherwise be trusted on the next scan and silently record a foreign
/// blob — the data-integrity bug this guards. Stat after the write *and* any
/// permission change, since chmod also bumps ctime.
pub(crate) fn stat_cache_upsert(
    rel_path: &str,
    abs_path: &Path,
    blob_hash: &Hash,
) -> Option<(String, StatCacheEntry)> {
    let md = fs::metadata(abs_path).ok()?;
    let (size, mtime_ns, ctime_ns) = file_stat_for_cache(&md);
    Some((
        rel_path.to_string(),
        StatCacheEntry {
            mtime_ns: mtime_ns?,
            ctime_ns,
            size,
            blob_hash: blob_hash.clone(),
        },
    ))
}

/// Refresh the stat cache after a materialization wrote `upserts` to the
/// working tree, dropping rows for paths no longer present so the cache stays
/// a faithful mirror of the on-disk tree. Mirrors [`scan_working_dir`]'s
/// phase-3 pruning.
///
/// Pass one upsert per file the materializer actually wrote; any cached path
/// not in `upserts` is treated as vanished and pruned. Callers that only *add*
/// files without deleting (the conflict write-out) should instead upsert
/// directly with an empty `removed` set.
pub(crate) fn refresh_stat_cache_after_materialize(
    repo: &dyn Repository,
    upserts: Vec<(String, StatCacheEntry)>,
) -> Result<()> {
    let upserted: HashSet<&str> = upserts.iter().map(|(p, _)| p.as_str()).collect();
    let removed: Vec<String> = repo
        .load_stat_cache()?
        .into_keys()
        .filter(|k| !upserted.contains(k.as_str()))
        .collect();
    repo.update_stat_cache(&upserts, &removed)
}

/// Options that can tweak commit behavior beyond message + path.
///
/// Kept as a struct (not extra `run_*` variants) so callsites that don't care
/// — primarily tests — stay on the simpler [`run`] entry point and aren't
/// disturbed when we add new toggles.
#[derive(Debug, Default, Clone)]
pub struct CommitOptions {
    /// Skip both `pre-commit` and `post-commit` hooks. Mirrors `git commit
    /// --no-verify`.
    pub no_verify: bool,

    /// Commit only changes under these paths (files or directories, absolute
    /// or cwd-relative). Empty — the default — commits every change.
    pub paths: Vec<PathBuf>,

    /// After creating the local commit, explicitly publish pending branch
    /// commits. Plain `oak commit` never contacts the remote.
    pub push: bool,

    /// Emit machine-readable JSON instead of human/piped commit output.
    pub json: bool,

    /// Suppress human success/no-op text. JSON output is still emitted when
    /// `json` is true.
    pub quiet: bool,
}

#[derive(Debug, Serialize)]
pub struct CommitJson {
    pub schema_version: u32,
    pub committed: bool,
    pub pushed: bool,
    pub published: bool,
    pub remote_contacted: bool,
    pub branch: String,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub change_counts: output::StatusChangeCountsJson,
    pub paths_sample: Vec<String>,
    pub paths_sample_omitted: usize,
    pub unpushed_commit_count: usize,
    pub next_commands: Vec<String>,
}

#[derive(Debug)]
struct CommitOutcome {
    committed: bool,
    pushed: bool,
    published: bool,
    remote_contacted: bool,
    branch: String,
    head_before: Option<String>,
    head_after: Option<String>,
    changes: Vec<FileChange>,
    unpushed_commit_count: usize,
}

impl CommitOutcome {
    fn json(&self) -> CommitJson {
        let paths_sample: Vec<String> = self
            .changes
            .iter()
            .take(output::COMPACT_CHANGE_LIMIT)
            .map(|change| change.path.clone())
            .collect();
        CommitJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            committed: self.committed,
            pushed: self.pushed,
            published: self.published,
            remote_contacted: self.remote_contacted,
            branch: self.branch.clone(),
            head_before: self.head_before.clone(),
            head_after: self.head_after.clone(),
            change_counts: commit_change_counts(&self.changes),
            paths_sample,
            paths_sample_omitted: self
                .changes
                .len()
                .saturating_sub(output::COMPACT_CHANGE_LIMIT),
            unpushed_commit_count: self.unpushed_commit_count,
            next_commands: commit_next_commands(self.unpushed_commit_count, self.pushed),
        }
    }
}

/// Create a commit within the current branch using default options.
///
/// Local commits no longer carry messages — the source of truth for "what
/// happened" is the branch description, which becomes the (single) main
/// commit's message at squash-merge time on the server.
pub fn run(path: &Path) -> Result<()> {
    run_with_options(path, CommitOptions::default())
}

/// Variant of [`run`] that accepts behavioral toggles.
pub fn run_with_options(path: &Path, opts: CommitOptions) -> Result<()> {
    let no_verify = opts.no_verify;
    let push_after_commit = opts.push;
    output::vlog("commit: start");
    // `oak commit` somewhere with no repository: rather than erroring out,
    // offer to initialize one right here (interactive only). This is the same
    // courtesy `oak status` extends, and it lets a brand-new project go from
    // `oak commit` straight to a tracked repo — and, after the commit, an
    // offer to push it up as a new repo on the server.
    let ctx = match crate::resolve::resolve(path) {
        Ok(ctx) => ctx,
        Err(OakError::RepoNotFound) => match offer_init_here(path)? {
            Some(ctx) => ctx,
            None => return Err(OakError::RepoNotFound),
        },
        Err(e) => return Err(e),
    };
    let work_tree = ctx.work_tree.clone();
    let _lock =
        crate::workdir_lock::WorkdirLock::acquire_wait(&ctx.oak_dir, Duration::from_secs(2))?;

    // Run pre-commit hook BEFORE opening the repo so a hook that mutates
    // working-tree files (e.g. `cargo fmt`) is reflected in the scan below.
    // We hold the workdir lock across the hook so two parallel commits can't
    // race on the same scripts.
    if !no_verify {
        hooks::run_hook(&work_tree, "pre-commit", true)?;
    }

    let repo = ctx.open()?;
    output::vlog("commit: opened repo + acquired workdir lock");

    // Get branch name from DB / git HEAD
    let branch_name = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;

    // Verify the branch exists and is open. On the git backend `get_branch` always
    // returns Open (no metadata sidecar yet) so this never trips there.
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;

    if branch.status == BranchStatus::Closed {
        return Err(OakError::BranchClosed(branch_name.clone()));
    }

    let ignore = IgnorePatterns::new(&work_tree)?;

    // Scan working directory
    let entries = scan_working_dir(&work_tree, &work_tree, repo.as_ref(), &ignore)?;
    output::vlog(&format!(
        "commit: scanned working dir ({} entries)",
        entries.len()
    ));

    // Get branch head as parent hash. If this branch has no commits yet,
    // inherit the parent branch's head so the first commit diffs against it
    // and is recorded as its child.
    let parent_hash = resolve_effective_head(repo.as_ref(), &branch_name)?;

    // Whether this is the first commit reachable from the current branch.
    // Plain commits stay local even in this case; the flag only feeds verbose
    // diagnostics now that publishing is explicit.
    let is_first_commit = parent_hash.is_none();

    // Get old manifest for comparison
    let old_manifest = if let Some(ref head_hash) = parent_hash {
        let head_commit = repo
            .get_commit(head_hash)?
            .ok_or_else(|| OakError::CommitNotFound(head_hash.to_string()))?;
        repo.get_manifest(&head_commit.manifest_hash)?
            .unwrap_or_else(Manifest::empty)
    } else {
        Manifest::empty()
    };

    let new_manifest = Manifest::new(entries);

    // Compute file changes. The manifest-level diff catches exact-hash
    // renames (pure moves with no edit); content-similarity below promotes
    // any rename-with-edit pairs in the same pass.
    let mut changes = old_manifest.diff(&new_manifest);
    detect_renames_by_content(
        &mut changes,
        |h| Ok(repo.get_blob(h)?.map(|b| b.content)),
        &RenameConfig::default(),
    )?;
    output::vlog(&format!(
        "commit: diffed manifest ({} changes)",
        changes.len()
    ));

    // Check if there are any changes
    if changes.is_empty() {
        let mut outcome = CommitOutcome {
            committed: false,
            pushed: false,
            published: false,
            remote_contacted: false,
            branch: branch_name.clone(),
            head_before: parent_hash.as_ref().map(|h| h.to_string()),
            head_after: parent_hash.as_ref().map(|h| h.to_string()),
            changes: Vec::new(),
            unpushed_commit_count: unmerged_commit_count(repo.as_ref(), &branch_name)?,
        };
        finalize_commit_outcome(
            &work_tree,
            repo.as_ref(),
            &opts,
            &mut outcome,
            0,
            Some("Nothing to commit"),
        )?;
        return Ok(());
    }

    // Scope the commit to the user's paths, if any were given. Selection uses
    // the exact filter `oak diff <paths>` applies, so a scoped diff and a
    // scoped commit always agree on which files are in. Whatever isn't
    // selected keeps its parent-commit state in the new manifest and stays a
    // working-tree change for a later commit.
    let filters = super::diff::resolve_filters(path, &work_tree, &opts.paths)?;
    let unselected = {
        let total = changes.len();
        changes = super::diff::filter_changes(changes, &filters);
        total - changes.len()
    };
    if changes.is_empty() {
        let mut outcome = CommitOutcome {
            committed: false,
            pushed: false,
            published: false,
            remote_contacted: false,
            branch: branch_name.clone(),
            head_before: parent_hash.as_ref().map(|h| h.to_string()),
            head_after: parent_hash.as_ref().map(|h| h.to_string()),
            changes: Vec::new(),
            unpushed_commit_count: unmerged_commit_count(repo.as_ref(), &branch_name)?,
        };
        finalize_commit_outcome(
            &work_tree,
            repo.as_ref(),
            &opts,
            &mut outcome,
            unselected,
            Some("No changes to commit in the given paths"),
        )?;
        return Ok(());
    }

    // Tracked files dropped solely because they're now ignored get a loud
    // callout: in the change list they look like ordinary deletions, and on a
    // big commit an over-broad ignore pattern can silently sweep thousands of
    // real files off the branch.
    warn_tracked_now_ignored(
        &changes,
        &work_tree,
        "will be removed from the branch by this commit",
    );

    // No commit message: under the new model, branch descriptions are the
    // source of truth for "what happened." A real message only gets
    // attached when the server squash-merges a feature branch into main.

    // Store the manifest and commit using the backend's canonical hashes
    // (BLAKE3 for sqlite, tree/commit OIDs for git), then advance refs through
    // one backend-aware transition. A scoped commit stores the parent manifest
    // with only the selected changes applied, not the full working-tree snapshot.
    let entries = if unselected == 0 {
        new_manifest.entries.clone()
    } else {
        scoped_manifest_entries(&old_manifest, &new_manifest, &changes)
    };

    let files: Vec<FileChange> = changes
        .iter()
        .map(|c| FileChange {
            path: c.path.clone(),
            change_type: c.change_type,
            old_blob_hash: c.old_blob_hash.clone(),
            new_blob_hash: c.new_blob_hash.clone(),
            old_path: c.old_path.clone(),
            old_mode: None,
            new_mode: None,
        })
        .collect();

    let commit_hash = repo.put_commit_and_advance_refs(
        branch_name.clone(),
        parent_hash.clone(),
        None,
        entries,
        get_author(),
        None,
        chrono::Utc::now(),
        files,
    )?;
    output::vlog("commit: stored commit object and updated refs");

    let mut outcome = CommitOutcome {
        committed: true,
        pushed: false,
        published: false,
        remote_contacted: false,
        branch: branch_name.clone(),
        head_before: parent_hash.as_ref().map(|h| h.to_string()),
        head_after: Some(commit_hash.to_string()),
        changes: changes.clone(),
        unpushed_commit_count: unmerged_commit_count(repo.as_ref(), &branch_name)?,
    };

    finalize_commit_outcome(
        &work_tree,
        repo.as_ref(),
        &opts,
        &mut outcome,
        unselected,
        None,
    )?;

    // Post-commit hook (non-blocking — the commit already landed, so a hook
    // failure here only warns).
    if !no_verify {
        let _ = hooks::run_hook(&work_tree, "post-commit", false);
    }

    if is_first_commit && !push_after_commit {
        output::vlog("commit: first local commit saved without publishing");
    }

    Ok(())
}

/// Manifest entries for a path-scoped commit: the parent commit's entries
/// with only the selected changes applied. Unselected paths keep their
/// parent-commit state — the working tree still differs from the new head
/// there, so `oak status` keeps reporting them after the commit.
fn scoped_manifest_entries(
    old_manifest: &Manifest,
    new_manifest: &Manifest,
    selected: &[FileChange],
) -> Vec<ManifestEntry> {
    let new_by_path: HashMap<&str, &ManifestEntry> = new_manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let mut merged: BTreeMap<String, ManifestEntry> = old_manifest
        .entries
        .iter()
        .map(|e| (e.path.clone(), e.clone()))
        .collect();
    for change in selected {
        // A rename retires its old path along with landing the new one.
        if let Some(old) = change.old_path.as_deref() {
            merged.remove(old);
        }
        if change.change_type == ChangeType::Deleted {
            merged.remove(&change.path);
        } else if let Some(entry) = new_by_path.get(change.path.as_str()) {
            merged.insert(entry.path.clone(), (*entry).clone());
        }
    }
    merged.into_values().collect()
}

fn finalize_commit_outcome(
    work_tree: &Path,
    repo: &dyn Repository,
    opts: &CommitOptions,
    outcome: &mut CommitOutcome,
    unselected: usize,
    no_change_message: Option<&str>,
) -> Result<()> {
    if !opts.json && !opts.quiet && outcome.committed && !std::io::stdout().is_terminal() {
        // The hash is the only information a piped reader can't already know.
        if let Some(head) = &outcome.head_after {
            output::print_line(Hash(head.clone()).short());
        }
    } else if !opts.json && !opts.quiet {
        if outcome.committed {
            print_commit_human(outcome, unselected);
        } else if let Some(message) = no_change_message {
            output::info(message);
        }
    }

    if opts.push && outcome.unpushed_commit_count > 0 {
        outcome.remote_contacted = true;
        push_pending_commits(work_tree, repo, opts.json || opts.quiet)?;
        outcome.pushed = true;
        outcome.published = true;
        outcome.unpushed_commit_count = 0;
    }

    if opts.json {
        output::print_json(&outcome.json())?;
    } else if !opts.quiet
        && outcome.committed
        && !opts.push
        && outcome.unpushed_commit_count > 0
        && std::io::stdout().is_terminal()
    {
        output::info(
            "Committed locally. Run `oak push` or `oak finish --desc-file <file> --json` to publish.",
        );
    }

    Ok(())
}

fn print_commit_human(outcome: &CommitOutcome, unselected: usize) {
    let short = outcome
        .head_after
        .as_deref()
        .map(|head| Hash(head.to_string()).short().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    output::success(&format!(
        "Created commit {}{}{} on branch {}{}{}",
        output::colors::CYAN,
        short,
        output::colors::RESET,
        output::colors::CYAN,
        outcome.branch,
        output::colors::RESET,
    ));

    for change in &outcome.changes {
        if change.change_type == ChangeType::Renamed {
            if let Some(ref old_path) = change.old_path {
                output::item(&output::format_rename(old_path, &change.path));
            } else {
                output::item(&output::format_change_type(
                    change.change_type,
                    &change.path,
                ));
            }
        } else {
            output::item(&output::format_change_type(
                change.change_type,
                &change.path,
            ));
        }
    }

    if unselected > 0 {
        output::info(&format!(
            "{unselected} other change{} left uncommitted (see `oak status`)",
            if unselected == 1 { "" } else { "s" },
        ));
    }
}

fn push_pending_commits(
    work_tree: &Path,
    repo: &dyn Repository,
    suppress_output: bool,
) -> Result<()> {
    let remote = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .unwrap_or_else(|| {
            std::env::var("OAK_REMOTE").unwrap_or_else(|_| "https://oak.space".to_string())
        });
    output::vlog(&format!("commit: explicit --push to {remote}"));
    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
    if suppress_output {
        output::begin_capture();
        let result = rt.block_on(push::run(work_tree, &remote, false, None));
        let _captured = output::end_capture();
        result
    } else {
        output::print_line("");
        rt.block_on(push::run(work_tree, &remote, false, None))
    }
}

fn commit_change_counts(changes: &[FileChange]) -> output::StatusChangeCountsJson {
    let mut counts = output::StatusChangeCountsJson::default();
    for change in changes {
        match change.change_type {
            ChangeType::Added => counts.added += 1,
            ChangeType::Modified => counts.modified += 1,
            ChangeType::Deleted => counts.deleted += 1,
            ChangeType::Renamed => counts.renamed += 1,
        }
    }
    counts
}

fn commit_next_commands(unpushed_commit_count: usize, pushed: bool) -> Vec<String> {
    if unpushed_commit_count > 0 {
        vec![
            "oak push".to_string(),
            "oak finish --desc-file <file> --json".to_string(),
        ]
    } else if pushed {
        vec!["oak finish --desc-file <file> --json".to_string()]
    } else {
        vec!["oak status --json".to_string()]
    }
}

/// `oak commit` was run somewhere with no Oak repository. Explain the situation
/// and (interactive only) offer to initialize one right here, returning the
/// resolved context when the user accepts. Returns `Ok(None)` when declined or
/// when stdin isn't a terminal, so the caller surfaces the normal
/// `RepoNotFound` error (whose message points at `oak init` / `oak clone`).
fn offer_init_here(path: &Path) -> Result<Option<crate::resolve::RepoContext>> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Ok(None);
    }
    output::warning(&format!(
        "Not an Oak repository (no .oak found in {} or any parent).",
        path.display()
    ));
    let create = Confirm::new()
        .with_prompt("Initialize an Oak repository here so you can commit?")
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    if !create {
        return Ok(None);
    }
    output::blank();
    // Reached only past the stdin-terminal guard above, so init runs interactively.
    super::init::run(path, true)?;
    output::blank();
    Ok(Some(crate::resolve::resolve(path)?))
}

/// Get the current working directory status (for status command)
///
/// Tries to use the current branch head. If no branch is set,
/// falls back to the legacy get_head() for backward compatibility.
/// Resolve the effective head for a branch: its own head if present,
/// otherwise walk parent_branch until one has a head. Guards against cycles.
pub(crate) fn resolve_effective_head(repo: &dyn Repository, name: &str) -> Result<Option<Hash>> {
    let mut seen = std::collections::HashSet::new();
    let mut current = name.to_string();
    loop {
        if !seen.insert(current.clone()) {
            return Ok(None);
        }
        if let Some(h) = repo.get_branch_head(&current)? {
            return Ok(Some(h));
        }
        match repo.get_branch(&current)? {
            Some(b) => match b.parent_branch {
                Some(p) => current = p,
                None => return Ok(None),
            },
            None => return Ok(None),
        }
    }
}

/// How many commits on `branch_name` are waiting to be merged into its parent
/// — the branch's local work that a squash-merge onto `main` would carry.
///
/// This is just the number of commits authored on the branch (see
/// [`Repository::count_commits_for_branch`]): a branch starts seeded at its
/// parent's head with zero commits of its own, each `oak commit` adds one, and
/// merging squashes them onto the parent and drops you on a fresh branch with
/// zero again. It's deliberately offline — we don't diff against the local
/// `main` head, which is treated as unreliable/stale elsewhere (see `sync.rs`)
/// and would also force a network round-trip on the prompt's hot path.
pub(crate) fn unmerged_commit_count(repo: &dyn Repository, branch_name: &str) -> Result<usize> {
    repo.count_commits_for_branch(branch_name)
}

/// Tracked paths this change set records as deletions only because ignore
/// rules now cover them: the file is still on disk, but the working-tree scan
/// skipped it, so the manifest diff reads "tracked then, absent now" as a
/// deletion. Dropping such files is intentional — it's how a branch self-heals
/// after build artifacts get committed and then ignored — but it's also how an
/// over-broad pattern silently sweeps real files off the branch, so commit/
/// status/diff surface these loudly instead of letting them blend into the
/// change list. Paths gone from disk are excluded: those are genuine deletions
/// no matter what the ignore rules say.
pub fn tracked_now_ignored<'a>(
    changes: &'a [FileChange],
    work_tree: &Path,
    ignore: &IgnorePatterns,
) -> Vec<&'a str> {
    changes
        .iter()
        .filter(|c| c.change_type == ChangeType::Deleted)
        .filter(|c| ignore.is_path_or_ancestors_ignored(&c.path))
        // lstat, not stat: a now-ignored symlink still counts even when its
        // target dangles.
        .filter(|c| work_tree.join(&c.path).symlink_metadata().is_ok())
        .map(|c| c.path.as_str())
        .collect()
}

/// How many top-level groups the now-ignored warning lists before eliding.
const NOW_IGNORED_GROUP_LIMIT: usize = 5;

/// Warn (stderr) when `changes` would drop tracked-but-now-ignored files, with
/// a per-top-level-directory breakdown so a mass drop is legible at a glance.
/// `fate` finishes the sentence — "will be removed from the branch by this
/// commit" (commit) vs "… by the next commit" (status/diff). Best-effort: an
/// unreadable ignore file just suppresses the warning, never fails the command.
pub fn warn_tracked_now_ignored(changes: &[FileChange], work_tree: &Path, fate: &str) {
    if !changes
        .iter()
        .any(|change| change.change_type == ChangeType::Deleted)
    {
        return;
    }
    let Ok(ignore) = IgnorePatterns::new(work_tree) else {
        return;
    };
    let dropped = tracked_now_ignored(changes, work_tree, &ignore);
    if dropped.is_empty() {
        return;
    }

    // Group by top-level path component; files at the repo root group as
    // themselves.
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for path in &dropped {
        let key = match path.split_once('/') {
            Some((top, _)) => format!("{top}/"),
            None => (*path).to_string(),
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut groups: Vec<(String, usize)> = counts.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let total = dropped.len();
    let (count, verb) = if total == 1 {
        ("1 tracked file".to_string(), "is")
    } else {
        (format!("{} tracked files", with_thousands(total)), "are")
    };

    // Everything under a single top-level directory: one self-contained line
    // ("1,234 tracked files under target/ are …"). Otherwise a summary line
    // plus a per-directory breakdown.
    let msg = if groups.len() == 1 {
        let (name, _) = &groups[0];
        if name.ends_with('/') {
            format!("{count} under {name} {verb} now ignored and {fate}")
        } else {
            format!("{count} ({name}) {verb} now ignored and {fate}")
        }
    } else {
        let mut msg = format!("{count} {verb} now ignored and {fate}:");
        for (name, n) in groups.iter().take(NOW_IGNORED_GROUP_LIMIT) {
            if name.ends_with('/') {
                msg.push_str(&format!(
                    "\n  {name} ({} file{})",
                    with_thousands(*n),
                    if *n == 1 { "" } else { "s" }
                ));
            } else {
                msg.push_str(&format!("\n  {name}"));
            }
        }
        if groups.len() > NOW_IGNORED_GROUP_LIMIT {
            msg.push_str(&format!(
                "\n  ... and {} more",
                groups.len() - NOW_IGNORED_GROUP_LIMIT
            ));
        }
        msg
    };
    output::warning(&msg);
}

/// Format a count with thousands separators ("1234567" → "1,234,567") so a
/// mass drop reads at a glance.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn get_status(path: &Path) -> Result<(Vec<FileChange>, Option<Hash>, Option<String>)> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;
    compute_changes_for_status(repo.as_ref(), &work_tree)
}

/// Compute the working-tree change set against the effective head, honoring the
/// active project/team scope.
///
/// This shares the same scan + manifest-diff + rename-detection pipeline that
/// `oak status` uses, but persists dirty cache misses for callers that need
/// those blobs available locally. It mirrors what [`run_with_options`] performs
/// before committing, so status, diff, and commit agree by construction (the
/// regression test in `tests/` pins this down).
/// Returns `(changes, effective_head, current_branch_name)`.
pub fn compute_changes(
    repo: &dyn Repository,
    work_tree: &Path,
) -> Result<(Vec<FileChange>, Option<Hash>, Option<String>)> {
    let (changes, head, branch_name, _ephemeral_blobs) =
        compute_changes_inner(repo, work_tree, true)?;
    Ok((changes, head, branch_name))
}

/// Compute the same working-tree change set without persisting dirty blobs.
/// Used by read-only paths such as `oak status` and regular `oak diff`, whose
/// output can be computed from committed pre-image blobs plus current worktree
/// bytes and must not mutate the object store.
pub(crate) fn compute_changes_for_status(
    repo: &dyn Repository,
    work_tree: &Path,
) -> Result<(Vec<FileChange>, Option<Hash>, Option<String>)> {
    let (changes, head, branch_name, _ephemeral_blobs) =
        compute_changes_for_status_with_ephemeral(repo, work_tree)?;
    Ok((changes, head, branch_name))
}

/// Check cleanliness using the read-only status scan. This is for callers that
/// only need a boolean guard before moving refs or resetting the worktree; it
/// must not populate the blob store with dirty worktree content.
pub fn worktree_is_clean_without_storing_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
) -> Result<bool> {
    Ok(compute_changes_for_status(repo, work_tree)?.0.is_empty())
}

/// Status changes plus the ephemeral blob cache built while scanning the
/// working tree: `(changes, head, branch description, hash→bytes for blobs that
/// were hashed in-memory but not persisted)`.
type StatusChangesWithBlobs = (
    Vec<FileChange>,
    Option<Hash>,
    Option<String>,
    HashMap<Hash, Vec<u8>>,
);

pub(crate) fn compute_changes_for_status_with_ephemeral(
    repo: &dyn Repository,
    work_tree: &Path,
) -> Result<StatusChangesWithBlobs> {
    compute_changes_inner(repo, work_tree, false)
}

fn compute_changes_inner(
    repo: &dyn Repository,
    work_tree: &Path,
    store_misses: bool,
) -> Result<StatusChangesWithBlobs> {
    let ignore = IgnorePatterns::new(work_tree)?;

    // Determine the current head: prefer DB branch, fall back to legacy head.
    // For a branch with no commits of its own, walk parent_branch so status
    // diffs against the parent's manifest rather than an empty one.
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = match &branch_name {
        Some(name) => resolve_effective_head(repo, name)?,
        None => repo.get_head()?,
    };

    // Get old manifest for comparison
    let old_manifest = if let Some(ref head_hash) = head {
        // Try loading as a commit first (new model)
        if let Some(head_commit) = repo.get_commit(head_hash)? {
            repo.get_manifest(&head_commit.manifest_hash)?
                .unwrap_or_else(Manifest::empty)
        } else {
            // No commit found -- no manifest to compare against
            Manifest::empty()
        }
    } else {
        Manifest::empty()
    };

    let scan = if store_misses {
        scan_working_dir_inner(work_tree, work_tree, repo, &ignore, true, true)?
    } else {
        // Status hashes dirty cache misses without writing their blobs.
        scan_working_dir_for_changes(work_tree, work_tree, repo, &ignore)?
    };

    // Compute file changes. Same two-pass logic as commit: exact-hash
    // renames here, then content-similarity to catch rename-with-edit.
    let activity = output::activity("Comparing against HEAD...");
    let mut changes = old_manifest.diff_entries_with_renames(&scan.entries);
    activity.set_message("Checking rename candidates...");
    detect_renames_by_content(
        &mut changes,
        |h| {
            if let Some(content) = scan.ephemeral_blobs.get(h) {
                Ok(Some(content.clone()))
            } else {
                Ok(repo.get_blob(h)?.map(|b| b.content))
            }
        },
        &RenameConfig::default(),
    )?;

    Ok((changes, head, branch_name, scan.ephemeral_blobs))
}
