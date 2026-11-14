//! The one materializer: make the working directory match a manifest.
//!
//! `oak switch`, `oak reset`, `oak restore`, `oak pull`, and the merge/sync
//! conflict write-out all used to carry their own copy of "write this manifest
//! to disk / delete what shouldn't be there", and each copy had bugs the
//! others had fixed (symlink-following deletion, missing stat-cache refresh,
//! never-deleting pulls). They all route through [`apply_manifest`] now;
//! behavior differences between callers are expressed as [`ApplyOpts`], not
//! forks.

use crate::output;
use crate::workdir_lock::WorkdirLock;
use oak_core::{FileMode, IgnorePatterns, Manifest, ManifestEntry, OakError, Repository, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Which on-disk files [`apply_manifest`] deletes.
pub enum DeleteScope<'a> {
    /// Delete every file (matching the filter) that isn't in the target
    /// manifest — full materialization (switch, reset, restore).
    Untracked,
    /// Delete only paths tracked in `old` that are absent from the target —
    /// a fast-forward (pull) that must preserve the user's untracked files
    /// while still applying upstream deletions.
    TrackedRemoved { old: &'a Manifest },
    /// Delete nothing (`oak merge` conflict write-out).
    Nothing,
}

/// What to do when a manifest entry's blob isn't in local storage.
#[derive(Clone, Copy)]
pub enum MissingBlobs {
    /// Hard-error with `BlobNotFound`. A silent skip produces a partial tree
    /// that masquerades as "everything modified" — and the next unattended
    /// `oak commit` then records that partial tree. Erroring is the safe
    /// default.
    Error,
    /// Skip the entry, recording its path in [`ApplyReport::skipped`]. Only
    /// for explicit operator opt-in (`OAK_ALLOW_PARTIAL_CLONE`).
    Skip,
}

pub struct ApplyOpts<'a> {
    pub delete: DeleteScope<'a>,
    /// Restrict both writes and deletions to matching repo-relative paths
    /// (`oak reset PATH` / `oak restore PATHS...`). `None` = whole tree.
    pub filter: Option<&'a dyn Fn(&str) -> bool>,
    pub missing_blobs: MissingBlobs,
    /// Missing target entries whose stale on-disk path must be removed rather
    /// than preserved. Used for operator-declared historical loss: retaining a
    /// prior branch's bytes at the same tracked path would misrepresent the
    /// selected snapshot.
    pub remove_missing_paths: Option<&'a HashSet<String>>,
    /// Prune directories emptied by the deletions above. Full
    /// materializations sweep all empty dirs (Oak doesn't track empty dirs);
    /// fast-forwards prune only the parents of files they deleted.
    pub clean_empty_dirs: bool,
}

impl Default for ApplyOpts<'static> {
    /// Full materialization: the tree ends up exactly equal to the manifest.
    fn default() -> Self {
        ApplyOpts {
            delete: DeleteScope::Untracked,
            filter: None,
            missing_blobs: MissingBlobs::Error,
            remove_missing_paths: None,
            clean_empty_dirs: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    /// Paths skipped because their blob is missing (`MissingBlobs::Skip`).
    pub skipped: Vec<String>,
    /// Non-UTF-8 paths left in place during untracked deletion.
    pub skipped_non_utf8_deletions: Vec<String>,
    /// Paths deleted from the working tree, including the old spelling of a
    /// case-only rename on a case-insensitive filesystem (renamed, not
    /// deleted, since it is the target's file).
    pub deleted: Vec<String>,
    /// `(on-disk path, tracked path)` for each kept file the filesystem
    /// treats as the tracked path but would not respell in place: its bytes
    /// are the tracked file's, under the old spelling (see
    /// [`CaseRenames::respell`]). Each one is also warned about.
    pub kept_spellings: Vec<(String, String)>,
}

/// Make the working dir match `target`. Preserves proved-equal regular files,
/// writes other matching entries, deletes per [`DeleteScope`], prunes emptied directories, and updates
/// the stat cache. Classification never follows symlinks (lstat), so a
/// working-tree symlink pointing at a directory — even one outside the repo —
/// is treated as a single entry, never recursed into.
///
/// The `&WorkdirLock` witness is deliberately unused: requiring it makes
/// unlocked working-tree mutation a compile error rather than a race.
///
/// The update is recorded in `.oak/WORKTREE_UPDATE` for as long as it runs
/// (see [`crate::worktree_update`]), so a process killed part-way leaves a
/// tree that every later command recognizes as interrupted.
///
/// Called directly, the record is [`Recovery::Clear`]: recovery only clears
/// it. That is right for exactly one kind of caller — one writing toward the
/// snapshot the refs named before it started and still name (`oak reset`,
/// `oak restore`, hydration, an abort whose conflict state is removed only
/// afterwards) — because a partial result there is honestly uncommitted
/// state. A caller whose refs move, before or after the tree, must open its
/// own record first ([`crate::worktree_update::begin`], or
/// `WorktreeDiff::apply`) and keep it until the refs and the tree agree;
/// this call then nests inside it.
///
/// [`Recovery::Clear`]: crate::worktree_update::Recovery::Clear
pub fn apply_manifest(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
    opts: ApplyOpts<'_>,
) -> Result<ApplyReport> {
    let update = crate::worktree_update::begin(
        lock,
        repo,
        root,
        crate::worktree_update::Recovery::Clear,
        None,
    )?;
    let result = apply_manifest_unrecorded(lock, root, repo, target, opts);
    // A failed apply already reported its error and leaves the tree as it
    // always has; only a killed process leaves the record behind.
    update.finish();
    result
}

/// [`apply_manifest`] without opening a record: for the recovery that replays
/// one.
pub(crate) fn apply_manifest_unrecorded(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
    opts: ApplyOpts<'_>,
) -> Result<ApplyReport> {
    let crash = crate::worktree_update::CrashPoint::from_env();
    let mut written = 0usize;
    // Files are written here, inside `.oak/`, and renamed into place, so a
    // process killed mid-file leaves no temp file in the working tree.
    let staging = crate::worktree_update::Staging::for_update(lock);
    let activity = output::activity("Writing working tree...");
    let ignore = IgnorePatterns::new(root)?;
    // A sparse checkout scopes every materialization to the active cone: paths
    // outside it are never written, and the out-of-cone files an existing tree
    // already lacks are never treated as deletions. Loading it here makes every
    // caller (switch, reset, pull, restore, merge write-out) cone-aware for
    // free — out-of-cone manifest entries are skipped before their (withheld)
    // blobs are ever requested.
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(oak_core::MetadataKey::SparsePaths)?
            .as_deref(),
    );
    let matches =
        |p: &str| opts.filter.is_none_or(|f| f(p)) && cone.as_ref().is_none_or(|c| c.covers(p));
    let target_paths: HashSet<&str> = target.entries.iter().map(|e| e.path.as_str()).collect();
    let replaceable_paths = ReplaceablePaths::new(repo, &opts);

    let mut report = ApplyReport::default();

    // Zero-byte files first: the empty blob is the one object whose bytes are
    // implied by its hash, so we can always reconstruct it locally rather than
    // treating its absence as a torn snapshot. A server that shipped the
    // manifest but not the empty blob (a known migration gap) would otherwise
    // fail every materialization of a repo containing an empty file.
    oak_core::ensure_empty_blobs_in_manifest(repo, target)?;

    // Fail before touching the working tree. Otherwise a missing blob late in
    // the manifest can leave an abort/reset/switch half-applied.
    if matches!(opts.missing_blobs, MissingBlobs::Error) {
        require_blobs_matching(repo, target, &matches)?;
    }
    validate_worktree_paths(target, &matches)?;

    // File/directory transitions must be checked as a whole before writing
    // any entry. Only tracked blockers may be removed; a late untracked
    // obstruction must not leave earlier files partially updated.
    let (blockers, obstructions) = directory_transition_blockers(
        root,
        repo,
        target,
        &matches,
        opts.missing_blobs,
        &replaceable_paths,
    )?;
    // The planner refuses these before any record or write
    // (`worktree_update::classify`); this is the backstop for callers that
    // reach the materializer without a plan.
    if let Some(obstruction) = obstructions.first() {
        return Err(OakError::DirtyWorkingTree(obstruction.message(root)));
    }
    crash.reached("materialize:before");
    for path in blockers {
        fs::remove_file(root.join(path))?;
    }

    // Materialize every matching entry, collecting a fresh stat-cache row per
    // regular file. The cache is keyed by path only and shared across every
    // branch in the working dir, so a row left over from another branch's
    // version of a path would otherwise be trusted on the next scan and
    // silently record a foreign blob. Symlinks are never cached.
    let mut cache_upserts = Vec::new();
    // Restricted markers mean "never written under this hash"; every hash this
    // pass writes (or finds already on disk) is cleared below, so a later
    // missing file reads as an ordinary deletion again.
    let restricted = crate::commands::restricted::load_restricted_blobs(repo);
    let mut materialized_restricted: Vec<&str> = Vec::new();
    // Target paths this pass wrote or found already right: the only ones a
    // dropped path can be another spelling of. An entry outside the cone or
    // filter, or withheld, is not on disk as this tree's file; on a folding
    // filesystem its path may still resolve to a dropped in-cone entry.
    let mut materialized: HashSet<&str> = HashSet::new();
    for entry in &target.entries {
        if !matches(&entry.path) {
            continue;
        }
        let file_path = root.join(&entry.path);
        let blob = match repo.get_blob(&entry.blob_hash)? {
            Some(b) => b,
            None => match opts.missing_blobs {
                MissingBlobs::Skip => {
                    report.skipped.push(entry.path.clone());
                    if opts
                        .remove_missing_paths
                        .is_some_and(|paths| paths.contains(&entry.path))
                    {
                        match fs::symlink_metadata(&file_path) {
                            Ok(metadata) if metadata.is_dir() => {
                                return Err(OakError::DirtyWorkingTree(format!(
                                    "refusing to replace directory '{}' with an unavailable tracked file",
                                    file_path.display()
                                )));
                            }
                            Ok(_) => fs::remove_file(&file_path)?,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        report.deleted.push(entry.path.clone());
                    }
                    continue;
                }
                MissingBlobs::Error => {
                    return Err(OakError::IncompleteBlobData {
                        context: format!("working tree materialization for '{}'", entry.path),
                        missing: entry.blob_hash.to_string(),
                    });
                }
            },
        };
        if restricted.contains(entry.blob_hash.as_str()) {
            materialized_restricted.push(entry.blob_hash.as_str());
        }
        // Check every ancestor before even reading for preservation. Opening
        // only the final component with O_NOFOLLOW still follows a stale
        // symlink above it and can mistake external bytes for this tree's file.
        // Missing/withheld entries have already skipped this work above.
        create_parent_dirs(root, &entry.path)?;
        materialized.insert(entry.path.as_str());
        if let Some(md) = matching_regular_file(&file_path, entry.mode, &blob.content) {
            // Cache the metadata that was checked with these bytes, not a new
            // path stat that might already describe an external writer's edit.
            let (size, mtime_ns, ctime_ns) = crate::commands::commit::file_stat_for_cache(&md);
            if let Some(mtime_ns) = mtime_ns {
                cache_upserts.push((
                    entry.path.clone(),
                    oak_core::StatCacheEntry {
                        size,
                        mtime_ns,
                        ctime_ns,
                        blob_hash: entry.blob_hash.clone(),
                    },
                ));
            }
            continue;
        }
        materialize_path_for_apply(
            root,
            &staging,
            &file_path,
            entry.mode,
            &blob.content,
            &replaceable_paths,
        )?;
        written += 1;
        crash.wrote(written);
        if entry.mode != FileMode::Symlink {
            if let Some(u) = crate::commands::commit::stat_cache_upsert(
                &entry.path,
                &file_path,
                &entry.blob_hash,
            ) {
                cache_upserts.push(u);
            }
        }
    }

    crate::commands::restricted::clear_restricted_blobs(repo, materialized_restricted)?;

    let mut case_renames = CaseRenames::new(target).only(materialized);
    match opts.delete {
        DeleteScope::Untracked => {
            activity.set_message("Removing files no longer tracked...");
            delete_untracked(
                root,
                root,
                &ignore,
                &target_paths,
                &matches,
                &mut case_renames,
                &mut report,
            )?;
            // Killed here, a renamed file is on disk under its old spelling
            // with the target's bytes: never deleted, only not yet renamed.
            crash.reached("materialize:deleted");
            case_renames.respell(root, lock.oak_dir(), &mut cache_upserts, &mut report);
            if opts.clean_empty_dirs {
                activity.set_message("Pruning empty directories...");
                clean_empty_dirs(root, &ignore)?;
            }
        }
        DeleteScope::TrackedRemoved { old } => {
            activity.set_message("Removing files no longer tracked...");
            for entry in &old.entries {
                if !matches(&entry.path) || target_paths.contains(entry.path.as_str()) {
                    continue;
                }
                oak_core::validate_worktree_path(&entry.path)?;
                let parent = Path::new(&entry.path).parent().unwrap_or(Path::new(""));
                if !real_directory_chain(root, parent)? {
                    // An ancestor was replaced by a target file or symlink.
                    // The old descendant no longer belongs to this worktree;
                    // even lstat on its leaf would traverse that ancestor.
                    report.deleted.push(entry.path.clone());
                    continue;
                }
                let file_path = root.join(&entry.path);
                match fs::symlink_metadata(&file_path) {
                    // lstat: delete the file or symlink itself, never what a
                    // symlink points at — unless it is a target file under
                    // another spelling (case or Unicode form).
                    Ok(md) if !md.is_dir() => {
                        if !case_renames.claims(root, &entry.path) {
                            fs::remove_file(&file_path)?;
                        }
                    }
                    // Replaced by a real directory, or already gone — leave
                    // it for the next scan to report.
                    _ => {}
                }
                // Prune the cache row either way: the tracked file this row
                // described no longer exists at this path.
                report.deleted.push(entry.path.clone());
            }
            crash.reached("materialize:deleted");
            case_renames.respell(root, lock.oak_dir(), &mut cache_upserts, &mut report);
            if opts.clean_empty_dirs {
                activity.set_message("Pruning empty directories...");
                prune_emptied_dirs(root, &ignore, &report.deleted)?;
            }
        }
        DeleteScope::Nothing => {}
    }

    // A full materialization leaves the tree exactly equal to the manifest,
    // so the cache should mirror it: keep the rows we wrote, drop everything
    // else. Filtered or fast-forward applies leave untouched paths' rows
    // valid, so they only upsert what was written and prune what was deleted.
    activity.set_message("Refreshing stat cache...");
    let full_mirror =
        matches!(opts.delete, DeleteScope::Untracked) && opts.filter.is_none() && cone.is_none();
    if full_mirror {
        crate::commands::commit::refresh_stat_cache_after_materialize(repo, cache_upserts)?;
    } else {
        repo.update_stat_cache(&cache_upserts, &report.deleted)?;
    }

    crash.reached("materialize:after");
    Ok(report)
}

/// Fail with `IncompleteBlobData` unless every entry of `target` that
/// [`apply_manifest`] would write — inside the sparse cone — has its blob
/// locally. Lets a caller check before it publishes refs that the tree update
/// following them can't stop halfway.
pub(crate) fn require_blobs_for_apply(repo: &dyn Repository, target: &Manifest) -> Result<()> {
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(oak_core::MetadataKey::SparsePaths)?
            .as_deref(),
    );
    oak_core::ensure_empty_blobs_in_manifest(repo, target)?;
    require_blobs_matching(repo, target, &|p: &str| {
        cone.as_ref().is_none_or(|c| c.covers(p))
    })
}

fn require_blobs_matching(
    repo: &dyn Repository,
    target: &Manifest,
    matches: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let mut missing_blobs = Vec::new();
    for entry in &target.entries {
        if matches(&entry.path) && !repo.has_blob(&entry.blob_hash)? {
            missing_blobs.push((entry.blob_hash.clone(), entry.path.clone()));
        }
    }
    if missing_blobs.is_empty() {
        return Ok(());
    }
    Err(OakError::IncompleteBlobData {
        context: format_missing_blob_context(&missing_blobs),
        missing: format_missing_blob_hashes(&missing_blobs),
    })
}

fn format_missing_blob_context(missing: &[(oak_core::Hash, String)]) -> String {
    let mut paths: Vec<String> = missing.iter().map(|(_, path)| path.clone()).collect();
    paths.sort();
    paths.dedup();
    if paths.len() == 1 {
        format!("working tree materialization for '{}'", paths[0])
    } else {
        format!(
            "working tree materialization for {} path(s): {}",
            paths.len(),
            paths.join(", ")
        )
    }
}

fn format_missing_blob_hashes(missing_blobs: &[(oak_core::Hash, String)]) -> String {
    let mut missing: Vec<String> = missing_blobs
        .iter()
        .map(|(hash, _)| hash.to_string())
        .collect();
    missing.sort();
    missing.dedup();
    missing.join(", ")
}

/// Write one materialized entry to disk, replacing whatever is currently at
/// `file_path`. Symlink entries become real symlinks — their stored content is
/// the target path; regular/executable entries are written as files. Any
/// Existing files and symlinks are replaced by renaming a completed sibling temp
/// path into place, so a crash cannot leave the path missing between remove and
/// write. Existing non-empty directories are refused so file<->directory
/// transitions cannot silently delete unrelated user content.
/// Refuse the whole materialization, before anything is written, when a
/// matching entry of `target` names a path that must never come from a tree
/// (see [`oak_core::worktree_path_problem`]): a hook under `.oak/`, a `.git`
/// component, or a Windows drive prefix.
pub(crate) fn validate_worktree_paths(
    target: &Manifest,
    matches: &dyn Fn(&str) -> bool,
) -> Result<()> {
    for entry in &target.entries {
        if matches(&entry.path) {
            oak_core::validate_worktree_path(&entry.path)?;
        }
    }
    Ok(())
}

/// Create the directories above repo-relative `rel_path` under `root`,
/// first unlinking any ancestor that is a symlink. A tree that needs
/// `x/file` can't also have `x` as a symlink, so one on disk is stale — left
/// by an earlier tree, or a case-folded twin from this one — and creating
/// directories or writing through it would put tree content wherever it
/// points, outside the working tree. Only the link is removed, never its
/// target.
pub(crate) fn create_parent_dirs(root: &Path, rel_path: &str) -> Result<()> {
    let mut components: Vec<&str> = rel_path.split('/').filter(|c| !c.is_empty()).collect();
    components.pop();
    let mut dir = root.to_path_buf();
    for component in components {
        dir.push(component);
        match fs::symlink_metadata(&dir) {
            Ok(md) if md.file_type().is_symlink() => {
                // A Windows directory symlink is removed with `remove_dir`.
                if fs::remove_file(&dir).is_err() {
                    fs::remove_dir(&dir)?;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    fs::create_dir_all(root.join(rel_path).parent().unwrap_or(root))?;
    Ok(())
}

pub(crate) fn materialize_path(file_path: &Path, mode: FileMode, content: &[u8]) -> Result<()> {
    materialize_path_inner(file_path, mode, content, None, None)
}

/// Preserve only a proved, stable regular file. A miss (including any I/O
/// uncertainty) retains the existing replacement behavior. This is deliberately
/// scoped to manifest application, not the generic atomic writer.
#[cfg(unix)]
fn matching_regular_file(file_path: &Path, mode: FileMode, content: &[u8]) -> Option<fs::Metadata> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let expected_mode = match mode {
        FileMode::Regular => 0o644,
        FileMode::Executable => 0o755,
        FileMode::Symlink => return None,
    };
    let before = fs::symlink_metadata(file_path).ok()?;
    if !before.is_file()
        || before.nlink() != 1
        || before.mode() & 0o7777 != expected_mode
        || before.len() != content.len() as u64
    {
        return None;
    }
    // A path replaced by a symlink must not be followed; a raced-in FIFO must
    // not block opening. fstat rejects every non-regular descriptor before read.
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file_path)
        .ok()?;
    let observed = file.metadata().ok()?;
    let same = |a: &fs::Metadata, b: &fs::Metadata| {
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.mode() == b.mode()
            && a.nlink() == b.nlink()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    };
    if !observed.is_file() || !same(&before, &observed) {
        return None;
    }
    let mut buffer = [0u8; 64 * 1024];
    for expected in content.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..expected.len()]).ok()?;
        if buffer[..expected.len()] != *expected {
            return None;
        }
    }
    if file.read(&mut buffer[..1]).ok()? != 0 {
        return None;
    }
    if !same(&observed, &file.metadata().ok()?)
        || !same(&observed, &fs::symlink_metadata(file_path).ok()?)
    {
        return None;
    }
    // WorkdirLock excludes cooperating Oak writers. Noncooperating writes
    // after this observation remain possible, as after an atomic replacement;
    // retaining this metadata makes subsequent edits invalidate the stat cache.
    Some(observed)
}

#[cfg(not(unix))]
fn matching_regular_file(
    _file_path: &Path,
    _mode: FileMode,
    _content: &[u8],
) -> Option<fs::Metadata> {
    // No portable no-follow descriptor proof: retain the existing safe path.
    None
}

fn materialize_path_for_apply(
    root: &Path,
    staging: &crate::worktree_update::Staging,
    file_path: &Path,
    mode: FileMode,
    content: &[u8],
    replaceable_paths: &ReplaceablePaths<'_>,
) -> Result<()> {
    materialize_path_inner(
        file_path,
        mode,
        content,
        Some((root, replaceable_paths)),
        Some(staging),
    )
}

fn materialize_path_inner(
    file_path: &Path,
    mode: FileMode,
    content: &[u8],
    replaceable_dir_context: Option<(&Path, &ReplaceablePaths<'_>)>,
    staging: Option<&crate::worktree_update::Staging>,
) -> Result<()> {
    match fs::symlink_metadata(file_path) {
        Ok(md) if md.file_type().is_dir() => {
            if dir_has_entries(file_path)? {
                let can_replace = match replaceable_dir_context {
                    Some((root, replaceable_paths)) => {
                        let relative = file_path.strip_prefix(root).unwrap_or(file_path);
                        let tracked = match relative.to_str() {
                            Some(dir) => replaceable_paths.under(dir)?,
                            None => HashSet::new(),
                        };
                        directory_contains_only_replaceable_paths(file_path, root, &tracked)?
                    }
                    None => false,
                };
                if !can_replace {
                    return Err(OakError::DirtyWorkingTree(format!(
                        "refusing to replace non-empty directory '{}' with a tracked file",
                        file_path.display()
                    )));
                }
                fs::remove_dir_all(file_path)?;
            } else {
                fs::remove_dir(file_path)?;
            }
        }
        Err(_) => {}
        Ok(_) => {}
    }
    if mode == FileMode::Symlink {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(content);
            let link_then_rename = |tmp_path: &Path| -> Result<()> {
                let result = (|| -> Result<()> {
                    std::os::unix::fs::symlink(target, tmp_path)?;
                    fs::rename(tmp_path, file_path)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(tmp_path);
                }
                result
            };
            // Staged under `.oak/` like a regular file, with the same
            // fallback to a temp sibling.
            let staged = staging
                .and_then(|staging| staging.staged())
                .is_some_and(|tmp| link_then_rename(&tmp).is_ok());
            if !staged {
                link_then_rename(&match staging {
                    Some(staging) => staging.sibling(file_path)?,
                    None => crate::atomic_file::temp_sibling(file_path)?,
                })?;
            }
        }
        #[cfg(not(unix))]
        {
            // No portable symlink-to-anything on Windows; preserve the data as a
            // regular file holding the target path rather than failing the
            // checkout. (oak's shipped mount/Windows story is separate.)
            crate::atomic_file::write_atomic(file_path, content)?;
        }
    } else {
        let perm = match mode {
            FileMode::Executable => 0o755,
            _ => 0o644,
        };
        match staging {
            Some(staging) => {
                crate::atomic_file::write_worktree_file_staged(staging, file_path, content, perm)?
            }
            None => crate::atomic_file::write_worktree_file(file_path, content, perm)?,
        }
    }
    Ok(())
}

/// Tracked paths a file may replace a directory over: the files at HEAD plus
/// `TrackedRemoved`'s old manifest. A directory holding only these is
/// replaceable; anything else in it (untracked work) refuses the write. Only
/// consulted when a file lands on a non-empty directory, and then only for
/// that directory's subtree, so an ordinary materialization never flattens
/// HEAD.
struct ReplaceablePaths<'a> {
    repo: &'a dyn Repository,
    old: Option<&'a [ManifestEntry]>,
}

impl<'a> ReplaceablePaths<'a> {
    fn new(repo: &'a dyn Repository, opts: &ApplyOpts<'a>) -> Self {
        let old = match opts.delete {
            DeleteScope::TrackedRemoved { old } => Some(old.entries.as_slice()),
            _ => None,
        };
        ReplaceablePaths { repo, old }
    }

    fn entry(&self, path: &str) -> Result<Option<ManifestEntry>> {
        if let Some(entry) = self.old.and_then(|old| old.iter().find(|e| e.path == path)) {
            return Ok(Some(entry.clone()));
        }
        let Some(head) = self.repo.get_head()? else {
            return Ok(None);
        };
        let Some(commit) = self.repo.get_commit(&head)? else {
            return Ok(None);
        };
        Ok(self
            .repo
            .resolve_path_in_tree(&commit.manifest_hash, path)?
            .filter(|entry| entry.kind != oak_core::TreeEntryKind::Tree)
            .map(|entry| ManifestEntry {
                path: path.into(),
                blob_hash: entry.hash,
                mode: entry.mode,
            }))
    }

    // Path membership alone cannot authorize removing a tracked obstruction:
    // its bytes or executable bit may contain unsaved local work.
    fn unchanged(&self, root: &Path, path: &str) -> Result<bool> {
        use std::io::Read;
        let Some(entry) = self.entry(path)? else {
            return Ok(false);
        };
        let Some(blob) = self.repo.get_blob(&entry.blob_hash)? else {
            return Ok(false);
        };
        let path = root.join(path);
        let md = match fs::symlink_metadata(&path) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if md.file_type().is_symlink() {
            return Ok(entry.mode == FileMode::Symlink
                && fs::read_link(&path)?.as_os_str().as_encoded_bytes() == blob.content);
        }
        if !md.is_file() || entry.mode == FileMode::Symlink {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if (md.permissions().mode() & 0o111 != 0) != (entry.mode == FileMode::Executable) {
                return Ok(false);
            }
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Ok(false);
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content)?;
        Ok(content == blob.content)
    }

    /// The replaceable paths at or under `dir` (repo-relative).
    fn under(&self, dir: &str) -> Result<HashSet<String>> {
        let prefix = format!("{dir}/");
        let mut paths = HashSet::new();
        if let Some(head) = self.repo.get_head()? {
            if let Some(commit) = self.repo.get_commit(&head)? {
                if let Some(subtree) = self.repo.subtree_at_path(&commit.manifest_hash, dir)? {
                    let mut fetch = |h: &oak_core::Hash| {
                        self.repo
                            .get_tree(h)?
                            .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
                    };
                    paths.extend(
                        oak_core::flatten_tree(&subtree, &mut fetch)?
                            .into_iter()
                            .map(|entry| format!("{prefix}{}", entry.path)),
                    );
                }
            }
        }
        if let Some(old) = self.old {
            paths.extend(
                old.iter()
                    .filter(|entry| entry.path.starts_with(&prefix))
                    .map(|entry| entry.path.clone()),
            );
        }
        Ok(paths)
    }
}

/// Something of the user's standing where a transition needs the other kind
/// of entry. Never removed implicitly: ignored content included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Obstruction {
    /// A non-empty directory where the target writes a file, holding
    /// something other than tracked files of the snapshot being left
    /// (untracked or ignored content, an empty directory, a non-UTF-8 name).
    Directory(String),
    /// A tracked file inside such a directory, modified locally.
    ModifiedInDirectory(String),
    /// A file or symlink, untracked or modified, where the target needs a
    /// directory.
    NotADirectory(String),
}

impl Obstruction {
    pub(crate) fn path(&self) -> &str {
        match self {
            Obstruction::Directory(path)
            | Obstruction::ModifiedInDirectory(path)
            | Obstruction::NotADirectory(path) => path,
        }
    }

    fn message(&self, root: &Path) -> String {
        match self {
            Obstruction::Directory(path) => format!(
                "refusing to replace non-empty directory '{}' with a tracked file",
                root.join(path).display()
            ),
            Obstruction::ModifiedInDirectory(path) => {
                format!("refusing to replace directory containing modified tracked path '{path}'")
            }
            Obstruction::NotADirectory(path) => {
                format!("refusing to replace modified or untracked path '{path}' with a directory")
            }
        }
    }
}

/// Every file/directory obstruction the move to `target` meets, without
/// writing, sorted by path: the same check `apply_manifest` makes before
/// its first write, for the planner to refuse with before any record exists.
/// `old` holds the tracked files the move drops (besides HEAD's): those may
/// be replaced when unmodified. Entries whose blob is not local are skipped,
/// as the materializer skips (or refuses) them before this matters.
pub(crate) fn transition_obstructions(
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
    matches: &dyn Fn(&str) -> bool,
    old: Option<&[ManifestEntry]>,
) -> Result<Vec<Obstruction>> {
    let replaceable = ReplaceablePaths { repo, old };
    let (_, obstructions) = directory_transition_blockers(
        root,
        repo,
        target,
        matches,
        MissingBlobs::Skip,
        &replaceable,
    )?;
    Ok(obstructions)
}

fn directory_transition_blockers(
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
    matches: &dyn Fn(&str) -> bool,
    missing: MissingBlobs,
    replaceable: &ReplaceablePaths<'_>,
) -> Result<(HashSet<String>, Vec<Obstruction>)> {
    let mut blockers = HashSet::new();
    let mut obstructions = std::collections::BTreeMap::new();
    let mut checked_dirs: HashSet<String> = HashSet::new();
    for entry in &target.entries {
        if !matches(&entry.path)
            || (matches!(missing, MissingBlobs::Skip) && !repo.has_blob(&entry.blob_hash)?)
        {
            continue;
        }
        let mut relative = String::new();
        let components: Vec<_> = entry.path.split('/').collect();
        let mut parent_missing = false;
        for component in &components[..components.len() - 1] {
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(component);
            if checked_dirs.contains(&relative) {
                continue;
            }
            match fs::symlink_metadata(root.join(&relative)) {
                Ok(md) if md.is_dir() => {
                    checked_dirs.insert(relative.clone());
                }
                // Never inspect descendants through a link. The write path
                // replaces the link before creating its real directories.
                Ok(md) if md.file_type().is_symlink() => {
                    if !replaceable.unchanged(root, &relative)? {
                        obstructions.insert(
                            relative.clone(),
                            Obstruction::NotADirectory(relative.clone()),
                        );
                    }
                    parent_missing = true;
                    break;
                }
                Ok(md) if md.is_file() && replaceable.unchanged(root, &relative)? => {
                    blockers.insert(relative.clone());
                    parent_missing = true;
                    break;
                }
                Ok(_) => {
                    obstructions.insert(
                        relative.clone(),
                        Obstruction::NotADirectory(relative.clone()),
                    );
                    parent_missing = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    parent_missing = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if parent_missing {
            continue;
        }
        let path = root.join(&entry.path);
        match fs::symlink_metadata(&path) {
            Ok(md) if md.is_dir() && dir_has_entries(&path)? => {
                let tracked = replaceable.under(&entry.path)?;
                if !directory_contains_only_replaceable_paths(&path, root, &tracked)? {
                    obstructions.insert(
                        entry.path.clone(),
                        Obstruction::Directory(entry.path.clone()),
                    );
                    continue;
                }
                for tracked_path in &tracked {
                    // A tracked file already deleted is what the target
                    // has there too: nothing to lose.
                    if fs::symlink_metadata(root.join(tracked_path)).is_err() {
                        continue;
                    }
                    if !replaceable.unchanged(root, tracked_path)? {
                        obstructions.insert(
                            tracked_path.clone(),
                            Obstruction::ModifiedInDirectory(tracked_path.clone()),
                        );
                    }
                }
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    Ok((blockers, obstructions.into_values().collect()))
}

fn directory_contains_only_replaceable_paths(
    dir: &Path,
    root: &Path,
    replaceable_paths: &HashSet<String>,
) -> Result<bool> {
    let mut saw_entry = false;
    for entry in fs::read_dir(dir)? {
        saw_entry = true;
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !directory_contains_only_replaceable_paths(&path, root, replaceable_paths)? {
                return Ok(false);
            }
            continue;
        }
        let relative = path.strip_prefix(root).unwrap();
        // A non-UTF-8 name is never a tracked path.
        let Some(rel_str) = relative.to_str() else {
            return Ok(false);
        };
        if !replaceable_paths.contains(rel_str) {
            return Ok(false);
        }
    }
    Ok(saw_entry)
}

fn dir_has_entries(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)?;
    Ok(entries.next().transpose()?.is_some())
}

/// Delete files not in the manifest (and matching the caller's filter),
/// recording what was removed. A file that is a target path under another
/// spelling (case or Unicode form) is left for [`CaseRenames::respell`].
fn delete_untracked(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
    target_paths: &HashSet<&str>,
    matches: &dyn Fn(&str) -> bool,
    case_renames: &mut CaseRenames<'_>,
    report: &mut ApplyReport,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();
        // Classify without following symlinks: a symlink (even one pointing at a
        // directory) is a tracked entry in its own right, never a directory to
        // recurse into — descending through it would delete the *target's*
        // contents.
        let file_type = entry.file_type()?;
        let is_real_dir = file_type.is_dir();
        if ignore.is_ignored(relative, is_real_dir) {
            continue;
        }
        let Some(rel_str) = relative.to_str() else {
            let display = relative.display().to_string();
            output::warning(&format!(
                "leaving non-UTF-8 path '{display}' while removing untracked files"
            ));
            report.skipped_non_utf8_deletions.push(display);
            continue;
        };
        if is_real_dir {
            delete_untracked(
                &path,
                root,
                ignore,
                target_paths,
                matches,
                case_renames,
                report,
            )?;
        } else {
            if !target_paths.contains(rel_str) && matches(rel_str) {
                if !case_renames.claims(root, rel_str) {
                    fs::remove_file(&path)?;
                }
                report.deleted.push(rel_str.to_string());
            }
        }
    }
    Ok(())
}

/// Renames between the working tree and the target that the filesystem
/// treats as one name: a different case (`Case.txt` / `case.txt`), a
/// different Unicode normalization form (NFC `café.txt` / NFD `café.txt`),
/// or both, of the file or of any parent directory.
///
/// On a filesystem that folds names (APFS and HFS+, the macOS default, fold
/// case *and* normalization) the two spellings name one directory entry.
/// Writing the target's spelling lands in the existing entry and keeps the
/// old name (rename(2) onto an alias doesn't change it), and
/// `create_dir_all` reuses the old directory, so deleting the spelling the
/// target dropped would delete the file the target just wrote (fb-661 for
/// normalization, after case). Before a deletion, [`Self::claims`] checks
/// whether the path is the very entry some target path resolves to; such a
/// path is kept and [`Self::respell`] renames it, and any parent directory
/// spelled differently, to the target's spelling. On a filesystem that folds
/// neither, the spellings are separate files and the dropped one is deleted
/// as before.
///
/// Which names alias is the filesystem's decision, not a folding rule's: the
/// target is indexed by the identity (device and inode) of the entry each of
/// its paths resolves to on disk, built once, for the first deletion
/// candidate that exists, with one `lstat` per target path. Every candidate
/// is then one `lstat` and a lookup, never a scan of the target. `oak merge
/// --continue` and `oak pull --continue`, which delete a conflicted merge's
/// dropped paths themselves, use it the same way through
/// [`Self::rename_kept`].
pub(crate) struct CaseRenames<'a> {
    target: &'a Manifest,
    /// Target entries by the on-disk entry they resolve to, built for the
    /// first deletion candidate.
    by_identity: Option<HashMap<EntryId, Vec<&'a ManifestEntry>>>,
    /// When set, the only target paths considered: those actually on disk as
    /// this tree's files (see [`Self::only`]).
    eligible: Option<HashSet<&'a str>>,
    /// Dropped paths kept, with the target entry each one is.
    kept: Vec<(String, &'a ManifestEntry)>,
}

impl<'a> CaseRenames<'a> {
    pub(crate) fn new(target: &'a Manifest) -> Self {
        CaseRenames {
            target,
            by_identity: None,
            eligible: None,
            kept: Vec::new(),
        }
    }

    /// Consider only the target paths in `paths`: the ones this pass wrote
    /// or verified. A target path that was not materialized (outside the
    /// sparse cone or filter, or withheld) can still resolve, on a folding
    /// filesystem, to a dropped entry inside the cone; pairing them would
    /// rename in-cone files out of the cone.
    pub(crate) fn only(mut self, paths: HashSet<&'a str>) -> Self {
        self.eligible = Some(paths);
        self
    }

    /// Consider only the target paths inside `cone`, the paths a sparse
    /// checkout holds on disk (no cone: every path).
    pub(crate) fn within_cone(self, cone: Option<&oak_core::SparseCone>) -> Self {
        match cone {
            None => self,
            Some(cone) => {
                let paths = self
                    .target
                    .entries
                    .iter()
                    .filter(|entry| cone.covers(&entry.path))
                    .map(|entry| entry.path.as_str())
                    .collect();
                self.only(paths)
            }
        }
    }

    /// Whether the non-directory at `path`, a path the target doesn't track,
    /// is a target file spelled differently. If so it must not be deleted;
    /// it is remembered for [`Self::respell`] or [`Self::rename_kept`].
    pub(crate) fn claims(&mut self, root: &Path, path: &str) -> bool {
        let on_disk = root.join(path);
        // Nothing there, a directory, or a file with several names: not one
        // entry that a target spelling could be.
        let Some(id) = single_entry_id(&on_disk) else {
            return false;
        };
        let target = self.target;
        let eligible = self.eligible.as_ref();
        let by_identity = self.by_identity.get_or_insert_with(|| {
            let mut by_identity: HashMap<EntryId, Vec<&ManifestEntry>> = HashMap::new();
            for entry in &target.entries {
                if eligible.is_some_and(|eligible| !eligible.contains(entry.path.as_str())) {
                    continue;
                }
                if let Some(id) = single_entry_id(&root.join(&entry.path)) {
                    by_identity.entry(id).or_default().push(entry);
                }
            }
            by_identity
        });
        let Some(candidates) = by_identity.get(&id) else {
            return false;
        };
        // Re-check identity now, and that the target path reaches it through
        // real directories only: a target path not written by this pass may
        // still sit under a symlink, and one entry reached through a link is
        // not a second spelling of it.
        let Some(entry) = candidates.iter().copied().find(|entry| {
            entry.path != path
                && same_file_entry(&on_disk, &root.join(&entry.path))
                && real_directory_chain(
                    root,
                    Path::new(&entry.path).parent().unwrap_or(Path::new("")),
                )
                .unwrap_or(false)
        }) else {
            return false;
        };
        self.kept.push((path.to_string(), entry));
        true
    }

    /// Rename every kept path to its target spelling, then re-stat the
    /// stat-cache rows the write loop recorded for those files: a rename
    /// changes ctime.
    fn respell(
        self,
        root: &Path,
        oak_dir: &Path,
        cache_upserts: &mut [(String, oak_core::StatCacheEntry)],
        report: &mut ApplyReport,
    ) {
        let (renamed, kept) = self.rename_kept_reporting(root, oak_dir);
        report
            .kept_spellings
            .extend(kept.into_iter().map(|(from, to)| (from, to.to_string())));
        let renamed: HashSet<&str> = renamed.into_iter().map(|(_, to)| to).collect();
        if renamed.is_empty() {
            return;
        }
        for (path, row) in cache_upserts.iter_mut() {
            if !renamed.contains(path.as_str()) {
                continue;
            }
            if let Some((_, fresh)) = crate::commands::commit::stat_cache_upsert(
                path,
                &root.join(path.as_str()),
                &row.blob_hash,
            ) {
                *row = fresh;
            }
        }
    }

    /// Rename every kept path to its target spelling, returning `(kept path,
    /// target path)` for each one renamed. A failed or no-op rename only
    /// warns — the content is already right, under the old spelling, which
    /// the next status reports.
    pub(crate) fn rename_kept(self, root: &Path, oak_dir: &Path) -> Vec<(String, &'a str)> {
        self.rename_kept_reporting(root, oak_dir).0
    }

    /// [`Self::rename_kept`], also returning `(kept path, target path)` for
    /// each file the filesystem would not respell in place (see
    /// [`rename_spelling`]): it stays, intact, under the kept spelling.
    #[allow(clippy::type_complexity)]
    /// The kept ones are also noted for this command's JSON document and
    /// recorded in `.oak/KEPT_SPELLINGS` (see [`record_kept_spellings`]).
    fn rename_kept_reporting(
        self,
        root: &Path,
        oak_dir: &Path,
    ) -> (Vec<(String, &'a str)>, Vec<(String, &'a str)>) {
        let mut renamed_dirs = HashMap::new();
        let mut renamed = Vec::new();
        let mut kept = Vec::new();
        for (from, entry) in self.kept {
            match respell_path(root, &from, &entry.path, &mut renamed_dirs) {
                Ok(true) => renamed.push((from, entry.path.as_str())),
                Ok(false) => {
                    output::warning(&format!(
                        "'{from}' keeps its spelling on disk: this filesystem treats it as the \
                         same name as the tracked '{}' but would not rename it in place; \
                         `oak status` reports the difference",
                        entry.path
                    ));
                    kept.push((from, entry.path.as_str()));
                }
                Err(error) => {
                    output::warning(&format!(
                        "could not rename '{from}' to '{}': {error}",
                        entry.path
                    ));
                    kept.push((from, entry.path.as_str()));
                }
            }
        }
        output::note_kept_spellings(kept.iter().map(|(from, to)| (from.as_str(), *to)));
        record_kept_spellings(oak_dir, root, &kept);
        (renamed, kept)
    }
}

/// Rename `from` to `to`, two spellings of one entry under `root`, one
/// component at a time so parent directories spelled differently follow.
/// A component is renamed only when both spellings resolve to the same
/// directory (or, for the last, the same file); anything else — a symlink,
/// two distinct directories — ends the walk. `renamed_dirs` holds the target
/// directory prefixes an earlier path already handled, and whether that
/// respell took effect. Returns `false` when some component kept its old
/// spelling because the filesystem would not respell it in place.
fn respell_path(
    root: &Path,
    from: &str,
    to: &str,
    renamed_dirs: &mut HashMap<String, bool>,
) -> std::io::Result<bool> {
    let from_parts: Vec<&str> = from.split('/').collect();
    let to_parts: Vec<&str> = to.split('/').collect();
    if from_parts.len() != to_parts.len() {
        return Ok(true);
    }
    let last = to_parts.len() - 1;
    let mut respelled = true;
    let mut dir = root.to_path_buf();
    for (i, (old, new)) in from_parts.iter().zip(&to_parts).enumerate() {
        let wanted = dir.join(new);
        if old != new {
            let prefix = to_parts[..=i].join("/");
            match renamed_dirs.get(&prefix) {
                Some(done) => respelled &= *done,
                None => {
                    let current = dir.join(old);
                    let same = if i == last {
                        same_file_entry(&current, &wanted)
                    } else {
                        same_directory(&current, &wanted)
                    };
                    if !same {
                        return Ok(respelled);
                    }
                    let done = rename_spelling(&dir, old, new)?;
                    respelled &= done;
                    if i != last {
                        renamed_dirs.insert(prefix, done);
                    }
                }
            }
        }
        dir = wanted;
    }
    Ok(respelled)
}

/// Rename the entry `old` in `dir` to `new`, another spelling of the same
/// entry, and report whether `dir` now lists it as `new`.
///
/// A case-insensitive APFS or HFS+ volume changes the stored name on such a
/// rename. Others treat it as a rename of the entry onto itself, a
/// successful no-op: case-sensitive APFS for an NFC/NFD pair, and volumes
/// that store every name decomposed (HFS+, ExFAT) for an NFC target. The
/// entry then keeps its old spelling, intact, and `false` is returned; it is
/// never moved through a temporary name, which a process killed or an editor
/// writing in between could strand or overwrite. Both spellings were just
/// proved to be this one entry, so the rename replaces nothing else.
fn rename_spelling(dir: &Path, old: &str, new: &str) -> std::io::Result<bool> {
    fs::rename(dir.join(old), dir.join(new))?;
    for entry in fs::read_dir(dir)? {
        if entry?.file_name() == std::ffi::OsStr::new(new) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The identity of the single-named non-directory at `path` (lstat, so a
/// symlink is itself): what two spellings of one entry share and two
/// distinct files, hard links included, never do.
#[cfg(unix)]
type EntryId = (u64, u64);
#[cfg(not(unix))]
type EntryId = ();

#[cfg(unix)]
fn single_entry_id(path: &Path) -> Option<EntryId> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::symlink_metadata(path).ok()?;
    (!md.is_dir() && md.nlink() == 1).then_some((md.dev(), md.ino()))
}

// No stable inode identity in std off unix: nothing is claimed, and dropped
// paths are deleted as they always were there.
#[cfg(not(unix))]
fn single_entry_id(_path: &Path) -> Option<EntryId> {
    None
}

/// Whether `a` and `b` are one directory entry: the same non-directory inode
/// (lstat, so a symlink is compared as itself) with a single link, so two
/// names reaching it are spellings of one name, not hard links.
#[cfg(unix)]
fn same_file_entry(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => !a.is_dir() && a.dev() == b.dev() && a.ino() == b.ino() && a.nlink() == 1,
        _ => false,
    }
}

/// Whether `a` and `b` are the same real directory (lstat: a symlink to one
/// is not). Directories can't be hard-linked, so one inode is one entry.
#[cfg(unix)]
fn same_directory(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.is_dir() && a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

// No stable inode identity in std off unix: keep deleting dropped paths.
#[cfg(not(unix))]
fn same_file_entry(_a: &Path, _b: &Path) -> bool {
    false
}

#[cfg(not(unix))]
fn same_directory(_a: &Path, _b: &Path) -> bool {
    false
}

/// `.oak/KEPT_SPELLINGS`: the `(on-disk path, tracked path)` pairs an update
/// left under their old spelling, so `oak status` and `oak commit` can tell
/// them from a rename the user made (on a folding filesystem the two look
/// the same: one entry, two names). Rewritten by every update that deletes
/// or respells: entries that no longer name one entry are dropped.
const KEPT_SPELLINGS_FILE: &str = "KEPT_SPELLINGS";

/// The pairs [`record_kept_spellings`] last wrote. Unreadable or absent
/// means none: the kept file is then reported as an ordinary rename.
pub(crate) fn recorded_kept_spellings(oak_dir: &Path) -> HashSet<(String, String)> {
    fs::read(oak_dir.join(KEPT_SPELLINGS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<(String, String)>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn record_kept_spellings(oak_dir: &Path, root: &Path, kept: &[(String, &str)]) {
    let before = recorded_kept_spellings(oak_dir);
    let mut pairs: Vec<(String, String)> = before
        .iter()
        .filter(|(on_disk, tracked)| {
            is_kept_spelling(root, on_disk, tracked) && spelled_on_disk(root, on_disk)
        })
        .cloned()
        .collect();
    for (on_disk, tracked) in kept {
        let pair = (on_disk.clone(), tracked.to_string());
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    pairs.sort();
    if pairs.len() == before.len() && pairs.iter().all(|pair| before.contains(pair)) {
        return;
    }
    let path = oak_dir.join(KEPT_SPELLINGS_FILE);
    let result = if pairs.is_empty() {
        fs::remove_file(&path)
    } else {
        let tmp = oak_dir.join(format!("{KEPT_SPELLINGS_FILE}.tmp"));
        serde_json::to_vec(&pairs)
            .map_err(std::io::Error::other)
            .and_then(|bytes| fs::write(&tmp, bytes))
            .and_then(|()| fs::rename(&tmp, &path))
    };
    if let Err(error) = result {
        if error.kind() != std::io::ErrorKind::NotFound {
            output::warning(&format!(
                "could not record kept spellings in {}: {error}",
                path.display()
            ));
        }
    }
}

/// Whether every component of `rel` is listed in its directory byte for
/// byte: the name is spelled this way on disk, not merely resolvable to it
/// (a folding volume resolves either spelling). Prunes a recorded kept
/// spelling the user has since respelled.
fn spelled_on_disk(root: &Path, rel: &str) -> bool {
    let mut dir = root.to_path_buf();
    for part in rel.split('/') {
        let listed = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .any(|entry| entry.file_name() == std::ffi::OsStr::new(part))
            })
            .unwrap_or(false);
        if !listed {
            return false;
        }
        dir.push(part);
    }
    true
}

/// Whether `on_disk` is the filesystem's own spelling of the tracked path
/// `tracked`: two different names that lstat resolves to one non-directory
/// entry with a single link. On a filesystem that keeps names distinct
/// (ext4 NFC vs NFD, case-sensitive volumes for case) the tracked name of a
/// real rename no longer resolves, and a hard link has two links, so neither
/// is mistaken for a kept spelling.
pub(crate) fn is_kept_spelling(root: &Path, on_disk: &str, tracked: &str) -> bool {
    on_disk != tracked && same_file_entry(&root.join(on_disk), &root.join(tracked))
}

/// Whether two paths name one filesystem entry (two spellings of a file or
/// of a directory), as opposed to two entries.
pub(crate) fn same_entry(a: &Path, b: &Path) -> bool {
    same_file_entry(a, b) || same_directory(a, b)
}

/// Remove empty directories (except ignored ones like .oak)
fn clean_empty_dirs(path: &Path, ignore: &IgnorePatterns) -> Result<()> {
    fn clean_recursive(dir: &Path, root: &Path, ignore: &IgnorePatterns) -> Result<bool> {
        let mut is_empty = true;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).unwrap();
            if relative.to_str().is_none() {
                is_empty = false;
                continue;
            }
            // A symlink counts as content here (and must not be followed): even
            // a symlink to a directory keeps its parent non-empty and is never
            // recursed into or `remove_dir`'d.
            let file_type = entry.file_type()?;
            let is_real_dir = file_type.is_dir();
            if ignore.is_ignored(relative, is_real_dir) {
                is_empty = false;
                continue;
            }
            if is_real_dir {
                let dir_empty = clean_recursive(&entry_path, root, ignore)?;
                if dir_empty {
                    fs::remove_dir(&entry_path).ok();
                } else {
                    is_empty = false;
                }
            } else {
                is_empty = false;
            }
        }
        Ok(is_empty)
    }
    clean_recursive(path, path, ignore)?;
    Ok(())
}

/// Prove every component is an actual directory without following symlinks.
/// A missing/non-directory component means this lexical descendant is gone
/// from the worktree, even if following a replacement link could find it.
pub(crate) fn real_directory_chain(root: &Path, relative: &Path) -> Result<bool> {
    let mut directory = root.to_path_buf();
    for component in relative.components() {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

/// Targeted version of [`clean_empty_dirs`] for fast-forwards: walk up from
/// each deleted path, removing now-empty parent directories, without touching
/// empty directories the user created elsewhere. Also used by the
/// conflict-resolution sync (`oak pull --continue`), which applies the
/// parent's deletions itself rather than through [`apply_manifest`].
pub(crate) fn prune_emptied_dirs(
    root: &Path,
    ignore: &IgnorePatterns,
    deleted: &[String],
) -> Result<()> {
    for rel in deleted {
        let mut dir = match Path::new(rel).parent() {
            Some(p) if !p.as_os_str().is_empty() => root.join(p),
            _ => continue,
        };
        while dir.starts_with(root) && dir != *root {
            let rel_dir = dir.strip_prefix(root).unwrap();
            if ignore.is_ignored(rel_dir, true) || !real_directory_chain(root, rel_dir)? {
                break;
            }
            match fs::read_dir(&dir) {
                Ok(mut it) => {
                    if it.next().is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
            if fs::remove_dir(&dir).is_err() {
                break;
            }
            match dir.parent() {
                Some(p) => dir = p.to_path_buf(),
                None => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{FileMode, Hash, Manifest, ManifestEntry, SqliteRepository};
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn tracked_removal_never_follows_a_newly_materialized_symlink_ancestor() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root.join("link/sub")).unwrap();
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(root.join("link/sub/child"), b"tracked").unwrap();
        fs::write(outside.join("sub/child"), b"outside sentinel").unwrap();
        let oak_dir = root.join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let old = Manifest::new(vec![ManifestEntry {
            path: "link/sub/child".into(),
            blob_hash: repo.put_blob(b"tracked".to_vec()).unwrap(),
            mode: FileMode::Regular,
        }]);
        let target = Manifest::new(vec![ManifestEntry {
            path: "link".into(),
            blob_hash: repo
                .put_blob(outside.as_os_str().as_encoded_bytes().to_vec())
                .unwrap(),
            mode: FileMode::Symlink,
        }]);
        apply_manifest(
            &lock,
            &root,
            &repo,
            &target,
            ApplyOpts {
                delete: DeleteScope::TrackedRemoved { old: &old },
                ..ApplyOpts::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read_link(root.join("link")).unwrap(), outside);
        assert_eq!(
            fs::read(outside.join("sub/child")).unwrap(),
            b"outside sentinel"
        );
        assert!(outside.join("sub").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn pruning_never_walks_a_symlink_ancestor() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let ignore = IgnorePatterns::new(&root).unwrap();
        prune_emptied_dirs(&root, &ignore, &["link/sub/deleted".into()]).unwrap();
        assert!(
            outside.join("sub").is_dir(),
            "pruned an external directory through a link"
        );
        assert_eq!(fs::read_link(root.join("link")).unwrap(), outside);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_maximum_length_filename_for_every_mode() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("x".repeat(255));
        for mode in [FileMode::Regular, FileMode::Executable, FileMode::Symlink] {
            materialize_path(&path, mode, b"target").unwrap();
            if mode == FileMode::Symlink {
                assert_eq!(fs::read_link(&path).unwrap(), Path::new("target"));
            } else {
                assert_eq!(fs::read(&path).unwrap(), b"target");
            }
        }
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn create_parent_dirs_never_writes_through_a_symlinked_ancestor() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        // A stale link left by an earlier tree (`x -> ../outside`).
        std::os::unix::fs::symlink(&outside, root.join("x")).unwrap();

        create_parent_dirs(&root, "x/sub/authorized_keys").unwrap();

        assert!(fs::symlink_metadata(root.join("x")).unwrap().is_dir());
        assert!(root.join("x/sub").is_dir());
        assert!(!outside.join("sub").exists(), "wrote through the symlink");
    }

    #[cfg(unix)]
    #[test]
    fn apply_never_preserves_equal_files_through_symlink_ancestors() {
        for tracked_delete in [false, true] {
            let temp = TempDir::new().unwrap();
            let root = temp.path().join("work");
            let outside = temp.path().join("outside");
            fs::create_dir(&root).unwrap();
            fs::create_dir_all(outside.join("sub")).unwrap();
            fs::write(outside.join("sub/file"), b"target").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("x")).unwrap();
            let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
            let lock = WorkdirLock::acquire(temp.path()).unwrap();
            let blob = repo.put_blob(b"target".to_vec()).unwrap();
            let target = Manifest::new(vec![ManifestEntry {
                path: "x/sub/file".into(),
                blob_hash: blob.clone(),
                mode: FileMode::Regular,
            }]);
            let link_blob = repo
                .put_blob(outside.as_os_str().as_encoded_bytes().to_vec())
                .unwrap();
            let old = Manifest::new(vec![ManifestEntry {
                path: "x".into(),
                blob_hash: link_blob,
                mode: FileMode::Symlink,
            }]);
            let old_root = repo.put_manifest(old.entries.clone()).unwrap();
            let head = repo
                .put_commit(
                    "topic".into(),
                    None,
                    None,
                    old_root,
                    "tester".into(),
                    None,
                    chrono::Utc::now(),
                    vec![],
                )
                .unwrap();
            repo.set_head(&head).unwrap();
            let opts = ApplyOpts {
                delete: if tracked_delete {
                    DeleteScope::TrackedRemoved { old: &old }
                } else {
                    DeleteScope::Untracked
                },
                ..ApplyOpts::default()
            };
            apply_manifest(&lock, &root, &repo, &target, opts).unwrap();
            assert!(fs::symlink_metadata(root.join("x")).unwrap().is_dir());
            assert_eq!(fs::read(root.join("x/sub/file")).unwrap(), b"target");
            assert_eq!(fs::read(outside.join("sub/file")).unwrap(), b"target");
        }
    }

    #[test]
    fn apply_manifest_refuses_trees_that_plant_oak_hooks() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let lock = WorkdirLock::acquire(temp.path()).unwrap();
        let blob = repo.put_blob(b"#!/bin/sh\necho pwned\n".to_vec()).unwrap();
        let manifest = Manifest::new(vec![ManifestEntry {
            path: ".oak/hooks/pre-commit".to_string(),
            blob_hash: blob,
            mode: FileMode::Executable,
        }]);
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();

        let error = apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default())
            .expect_err("planted hook must be refused");

        assert!(
            error.to_string().contains(".oak/hooks/pre-commit"),
            "{error}"
        );
        assert!(!root.join(".oak/hooks/pre-commit").exists());
    }

    #[cfg(unix)]
    fn preservation_fixture(mode: FileMode) -> (TempDir, SqliteRepository, WorkdirLock, Manifest) {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("work")).unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let lock = WorkdirLock::acquire(temp.path()).unwrap();
        let blob = repo.put_blob(b"target\n".to_vec()).unwrap();
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "file".into(),
            blob_hash: blob,
            mode,
        }]);
        repo.store_manifest(&manifest).unwrap();
        apply_manifest(
            &lock,
            &temp.path().join("work"),
            &repo,
            &manifest,
            ApplyOpts::default(),
        )
        .unwrap();
        (temp, repo, lock, manifest)
    }

    #[cfg(unix)]
    #[test]
    fn apply_preserves_exact_regular_and_executable_files_but_repairs_same_size_edits() {
        use std::os::unix::fs::MetadataExt;
        for mode in [FileMode::Regular, FileMode::Executable] {
            let (temp, repo, lock, manifest) = preservation_fixture(mode);
            let root = temp.path().join("work");
            let file = root.join("file");
            let before = fs::metadata(&file).unwrap();
            fs::write(root.join("remove-me"), "untracked").unwrap();
            apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
            let after = fs::metadata(&file).unwrap();
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.modified().unwrap(), after.modified().unwrap());
            assert!(!root.join("remove-me").exists());
            let cache = repo.load_stat_cache().unwrap();
            let cached = cache.get("file").unwrap();
            assert_eq!(cached.blob_hash, manifest.entries[0].blob_hash);
            assert_eq!(
                (cached.size, Some(cached.mtime_ns), cached.ctime_ns),
                crate::commands::commit::file_stat_for_cache(&after)
            );
            fs::write(&file, b"dirty!\n").unwrap();
            fs::File::open(&file)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
                .unwrap();
            apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
            assert_eq!(fs::read(&file).unwrap(), b"target\n");
            assert_ne!(fs::metadata(&file).unwrap().ino(), after.ino());
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_normalizes_all_permission_bits_before_preserving_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        for (mode, wanted) in [(FileMode::Regular, 0o644), (FileMode::Executable, 0o755)] {
            let (temp, repo, lock, manifest) = preservation_fixture(mode);
            let root = temp.path().join("work");
            let file = root.join("file");
            for dirty_mode in [
                0o600,
                0o666,
                0o777,
                wanted | 0o4000,
                wanted | 0o2000,
                wanted | 0o1000,
            ] {
                fs::set_permissions(&file, fs::Permissions::from_mode(dirty_mode)).unwrap();
                let before = fs::metadata(&file).unwrap();
                apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
                let after = fs::metadata(&file).unwrap();
                assert_eq!(after.mode() & 0o7777, wanted);
                assert_ne!(after.ino(), before.ino());
                assert_eq!(fs::read(&file).unwrap(), b"target\n");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_replaces_symlinks_hardlinks_and_special_files_without_reading_aliases() {
        use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};
        use std::os::unix::net::UnixListener;
        let (temp, repo, lock, manifest) = preservation_fixture(FileMode::Regular);
        let root = temp.path().join("work");
        let file = root.join("file");
        let outside = temp.path().join("outside");
        fs::write(&outside, b"target\n").unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&outside, &file).unwrap();
        apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
        assert!(fs::symlink_metadata(&file).unwrap().is_file());
        assert_eq!(fs::read(&outside).unwrap(), b"target\n");
        fs::remove_file(&file).unwrap();
        fs::hard_link(&outside, &file).unwrap();
        apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
        assert_ne!(
            fs::metadata(&outside).unwrap().ino(),
            fs::metadata(&file).unwrap().ino()
        );
        fs::write(&outside, b"alias!\n").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"target\n");
        fs::remove_file(&file).unwrap();
        let listener = UnixListener::bind(&file).unwrap();
        assert!(fs::symlink_metadata(&file).unwrap().file_type().is_socket());
        apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"target\n");
        drop(listener);
        fs::remove_file(&file).unwrap();
        let name = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"target\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_preservation_respects_sparse_scope_and_missing_blob_preflight() {
        use std::os::unix::fs::MetadataExt;
        let (temp, repo, lock, mut manifest) = preservation_fixture(FileMode::Regular);
        let root = temp.path().join("work");
        let before = fs::metadata(root.join("file")).unwrap();
        manifest.entries.push(ManifestEntry {
            path: "outside".into(),
            blob_hash: Hash("a".repeat(64)),
            mode: FileMode::Regular,
        });
        repo.set_metadata(oak_core::MetadataKey::SparsePaths, "file")
            .unwrap();
        fs::write(root.join("outside"), "preserve outside cone").unwrap();
        apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap();
        assert_eq!(fs::metadata(root.join("file")).unwrap().ino(), before.ino());
        assert_eq!(
            fs::read_to_string(root.join("outside")).unwrap(),
            "preserve outside cone"
        );
        repo.set_metadata(oak_core::MetadataKey::SparsePaths, "")
            .unwrap();
        let error =
            apply_manifest(&lock, &root, &repo, &manifest, ApplyOpts::default()).unwrap_err();
        assert!(matches!(error, OakError::IncompleteBlobData { .. }));
        assert_eq!(fs::metadata(root.join("file")).unwrap().ino(), before.ino());
    }

    #[test]
    fn missing_blob_error_names_path_and_hash() {
        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let missing = Hash("a".repeat(64));
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "core/src/tree.rs".to_string(),
            blob_hash: missing.clone(),
            mode: FileMode::Regular,
        }]);

        let err = match apply_manifest(&lock, temp.path(), &repo, &manifest, ApplyOpts::default()) {
            Ok(_) => panic!("missing blob should fail materialization"),
            Err(err) => err,
        };

        let msg = err.to_string();
        assert!(msg.contains(missing.as_str()), "unexpected error: {msg}");
        assert!(
            msg.contains("core/src/tree.rs"),
            "missing blob errors should identify the affected path: {msg}"
        );
    }

    #[test]
    fn missing_blob_preflight_does_not_partially_write_tree() {
        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let present = repo.put_blob(b"new first\n".to_vec()).unwrap();
        let missing = Hash("b".repeat(64));
        fs::write(temp.path().join("first.txt"), "old first\n").unwrap();
        let manifest = Manifest::new(vec![
            ManifestEntry {
                path: "first.txt".to_string(),
                blob_hash: present,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "second.txt".to_string(),
                blob_hash: missing,
                mode: FileMode::Regular,
            },
        ]);

        let err = apply_manifest(&lock, temp.path(), &repo, &manifest, ApplyOpts::default())
            .expect_err("missing blob should fail before writing any path");

        assert!(matches!(err, OakError::IncompleteBlobData { .. }));
        assert_eq!(
            fs::read_to_string(temp.path().join("first.txt")).unwrap(),
            "old first\n"
        );
        assert!(!temp.path().join("second.txt").exists());
    }

    #[test]
    fn materialize_path_refuses_to_replace_non_empty_directory() {
        let temp = TempDir::new().unwrap();
        let tracked_path = temp.path().join("tracked.txt");
        fs::create_dir(&tracked_path).unwrap();
        fs::write(tracked_path.join("scratch.txt"), b"keep me").unwrap();

        let err = materialize_path(&tracked_path, FileMode::Regular, b"new content")
            .expect_err("non-empty directory replacement must be refused");

        let msg = err.to_string();
        assert!(
            msg.contains("non-empty directory"),
            "unexpected error: {msg}"
        );
        assert!(tracked_path.join("scratch.txt").exists());
    }

    #[test]
    fn apply_manifest_handles_tracked_file_to_deep_directory() {
        for relative in ["p", "a/b"] {
            for tracked_delete in [false, true] {
                let temp = TempDir::new().unwrap();
                let oak_dir = temp.path().join(".oak");
                fs::create_dir(&oak_dir).unwrap();
                let lock = WorkdirLock::acquire(&oak_dir).unwrap();
                let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
                let blob = repo.put_blob(b"old".to_vec()).unwrap();
                let old = Manifest::new(vec![ManifestEntry {
                    path: relative.into(),
                    blob_hash: blob,
                    mode: FileMode::Regular,
                }]);
                let manifest = repo.put_manifest(old.entries.clone()).unwrap();
                let commit = repo
                    .put_commit(
                        "topic".into(),
                        None,
                        None,
                        manifest,
                        "tester".into(),
                        None,
                        chrono::Utc::now(),
                        vec![],
                    )
                    .unwrap();
                repo.set_head(&commit).unwrap();
                let old_path = temp.path().join(relative);
                fs::create_dir_all(old_path.parent().unwrap()).unwrap();
                fs::write(&old_path, b"old").unwrap();
                let blob = repo.put_blob(b"new".to_vec()).unwrap();
                let target = Manifest::new(vec![ManifestEntry {
                    path: format!("{relative}/deep/file"),
                    blob_hash: blob,
                    mode: FileMode::Regular,
                }]);
                let opts = ApplyOpts {
                    delete: if tracked_delete {
                        DeleteScope::TrackedRemoved { old: &old }
                    } else {
                        DeleteScope::Untracked
                    },
                    ..ApplyOpts::default()
                };
                apply_manifest(&lock, temp.path(), &repo, &target, opts).unwrap();
                assert_eq!(fs::read(old_path.join("deep/file")).unwrap(), b"new");
            }
        }
    }

    #[test]
    fn transition_preflight_preserves_dirty_tracked_ancestors_and_descendants() {
        for directory_to_file in [false, true] {
            for tracked_delete in [false, true] {
                let temp = TempDir::new().unwrap();
                let root = temp.path().join("work");
                fs::create_dir(&root).unwrap();
                let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
                let lock = WorkdirLock::acquire(temp.path()).unwrap();
                let old_path = if directory_to_file { "z/child" } else { "z" };
                let target_path = if directory_to_file { "z" } else { "z/child" };
                let blob = repo.put_blob(b"tracked".to_vec()).unwrap();
                let old = Manifest::new(vec![ManifestEntry {
                    path: old_path.into(),
                    blob_hash: blob.clone(),
                    mode: FileMode::Regular,
                }]);
                let manifest = repo.put_manifest(old.entries.clone()).unwrap();
                let head = repo
                    .put_commit(
                        "topic".into(),
                        None,
                        None,
                        manifest,
                        "tester".into(),
                        None,
                        chrono::Utc::now(),
                        vec![],
                    )
                    .unwrap();
                repo.set_head(&head).unwrap();
                fs::create_dir_all(root.join(old_path).parent().unwrap()).unwrap();
                fs::write(root.join(old_path), b"unsaved local edits").unwrap();
                fs::write(root.join("a"), b"before").unwrap();
                let target = Manifest::new(vec![
                    ManifestEntry {
                        path: "a".into(),
                        blob_hash: blob.clone(),
                        mode: FileMode::Regular,
                    },
                    ManifestEntry {
                        path: target_path.into(),
                        blob_hash: blob,
                        mode: FileMode::Regular,
                    },
                ]);
                let opts = ApplyOpts {
                    delete: if tracked_delete {
                        DeleteScope::TrackedRemoved { old: &old }
                    } else {
                        DeleteScope::Untracked
                    },
                    ..ApplyOpts::default()
                };
                assert!(apply_manifest(&lock, &root, &repo, &target, opts).is_err());
                assert_eq!(
                    fs::read(root.join(old_path)).unwrap(),
                    b"unsaved local edits"
                );
                assert_eq!(fs::read(root.join("a")).unwrap(), b"before");
                assert_eq!(repo.get_head().unwrap(), Some(head));
            }
        }
    }

    #[test]
    fn transition_preflight_preserves_earlier_files_and_untracked_blockers() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"before").unwrap();
        fs::write(root.join("z"), b"untracked").unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let lock = WorkdirLock::acquire(temp.path()).unwrap();
        let blob = repo.put_blob(b"after".to_vec()).unwrap();
        let target = Manifest::new(vec![
            ManifestEntry {
                path: "a".into(),
                blob_hash: blob.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "z/child".into(),
                blob_hash: blob,
                mode: FileMode::Regular,
            },
        ]);
        assert!(apply_manifest(&lock, &root, &repo, &target, ApplyOpts::default()).is_err());
        assert_eq!(fs::read(root.join("a")).unwrap(), b"before");
        assert_eq!(fs::read(root.join("z")).unwrap(), b"untracked");
    }

    #[test]
    fn apply_manifest_allows_tracked_directory_to_file_transition() {
        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

        let old_blob = repo.put_blob(b"old".to_vec()).unwrap();
        let old_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked/nested.txt".to_string(),
                blob_hash: old_blob,
                mode: FileMode::Regular,
            }])
            .unwrap();
        let old_commit = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                old_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_head(&old_commit).unwrap();
        fs::create_dir(temp.path().join("tracked")).unwrap();
        fs::write(temp.path().join("tracked/nested.txt"), b"old").unwrap();

        let new_blob = repo.put_blob(b"new file\n".to_vec()).unwrap();
        let target = Manifest::new(vec![ManifestEntry {
            path: "tracked".to_string(),
            blob_hash: new_blob,
            mode: FileMode::Regular,
        }]);

        apply_manifest(&lock, temp.path(), &repo, &target, ApplyOpts::default()).unwrap();

        assert_eq!(
            fs::read(temp.path().join("tracked")).unwrap(),
            b"new file\n"
        );
    }

    #[test]
    fn apply_manifest_refuses_directory_to_file_when_untracked_content_is_present() {
        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

        let old_blob = repo.put_blob(b"old".to_vec()).unwrap();
        let old_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked/nested.txt".to_string(),
                blob_hash: old_blob,
                mode: FileMode::Regular,
            }])
            .unwrap();
        let old_commit = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                old_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_head(&old_commit).unwrap();
        fs::create_dir(temp.path().join("tracked")).unwrap();
        fs::write(temp.path().join("tracked/nested.txt"), b"old").unwrap();
        fs::write(temp.path().join("tracked/scratch.txt"), b"keep me").unwrap();

        let new_blob = repo.put_blob(b"new file\n".to_vec()).unwrap();
        let target = Manifest::new(vec![ManifestEntry {
            path: "tracked".to_string(),
            blob_hash: new_blob,
            mode: FileMode::Regular,
        }]);

        let err = apply_manifest(&lock, temp.path(), &repo, &target, ApplyOpts::default())
            .expect_err("untracked content under replaced directory must block");

        let msg = err.to_string();
        assert!(
            msg.contains("non-empty directory"),
            "unexpected error: {msg}"
        );
        assert_eq!(
            fs::read(temp.path().join("tracked/scratch.txt")).unwrap(),
            b"keep me"
        );
    }

    /// Every file and directory under `root` except `.oak`, sorted, with
    /// directories suffixed `/`: the spelling actually on disk.
    fn on_disk(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                if rel == ".oak" {
                    continue;
                }
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    out.push(format!("{rel}/"));
                    walk(&path, root, out);
                } else {
                    out.push(rel);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// A tree holding `Case.txt`, `Same.txt` and `Dir/in.txt`, and a target
    /// that renames all three by case alone: `case.txt` with new content,
    /// `same.txt` and `dir/in.txt` unchanged.
    fn case_rename_fixture() -> (TempDir, SqliteRepository, WorkdirLock, Manifest, Manifest) {
        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let entries = |files: [(&str, &[u8]); 3]| {
            Manifest::new(
                files
                    .into_iter()
                    .map(|(path, content)| ManifestEntry {
                        path: path.to_string(),
                        blob_hash: repo.put_blob(content.to_vec()).unwrap(),
                        mode: FileMode::Regular,
                    })
                    .collect(),
            )
        };
        let old = entries([
            ("Case.txt", b"x\n"),
            ("Same.txt", b"s\n"),
            ("Dir/in.txt", b"i\n"),
        ]);
        let target = entries([
            ("case.txt", b"y\n"),
            ("same.txt", b"s\n"),
            ("dir/in.txt", b"i\n"),
        ]);
        apply_manifest(&lock, temp.path(), &repo, &old, ApplyOpts::default()).unwrap();
        (temp, repo, lock, old, target)
    }

    /// A case-only rename leaves the target's files on disk under the
    /// target's spelling, whether the move rewrites the whole tree or deletes
    /// only the dropped paths. On a case-insensitive filesystem (the macOS
    /// default) the old and new spellings are one directory entry, so this is
    /// a rename, not a write plus a deletion; on a case-sensitive one they are
    /// two files and the dropped one is deleted. The end state is the same.
    #[test]
    fn apply_manifest_keeps_case_only_renames() {
        for tracked_removed in [false, true] {
            let (temp, repo, lock, old, target) = case_rename_fixture();
            let root = temp.path();
            let delete = if tracked_removed {
                DeleteScope::TrackedRemoved { old: &old }
            } else {
                DeleteScope::Untracked
            };
            let opts = ApplyOpts {
                delete,
                ..ApplyOpts::default()
            };
            apply_manifest(&lock, root, &repo, &target, opts).unwrap();

            assert_eq!(
                on_disk(root),
                ["case.txt", "dir/", "dir/in.txt", "same.txt"],
                "tracked_removed: {tracked_removed}"
            );
            assert_eq!(fs::read(root.join("case.txt")).unwrap(), b"y\n");
            assert_eq!(fs::read(root.join("same.txt")).unwrap(), b"s\n");
            assert_eq!(fs::read(root.join("dir/in.txt")).unwrap(), b"i\n");
            // The stat cache describes the files as they now are (a rename
            // changes ctime), under the target's spelling only.
            let cache = repo.load_stat_cache().unwrap();
            let mut cached: Vec<&str> = cache.keys().map(String::as_str).collect();
            cached.sort_unstable();
            assert_eq!(cached, ["case.txt", "dir/in.txt", "same.txt"]);
            for entry in &target.entries {
                let row = &cache[&entry.path];
                assert_eq!(row.blob_hash, entry.blob_hash);
                let md = fs::metadata(root.join(&entry.path)).unwrap();
                assert_eq!(
                    (row.size, Some(row.mtime_ns), row.ctime_ns),
                    crate::commands::commit::file_stat_for_cache(&md),
                    "{} tracked_removed: {tracked_removed}",
                    entry.path
                );
            }
        }
    }

    /// Whether this filesystem treats NFC and NFD spellings of a name as one
    /// entry (APFS and HFS+ do), probed where the test's files live.
    #[cfg(unix)]
    fn folds_unicode_normalization(dir: &Path) -> bool {
        let probe = dir.join("probe-caf\u{e9}");
        fs::write(&probe, b"").unwrap();
        let folds = dir.join("probe-cafe\u{301}").exists();
        fs::remove_file(probe).unwrap();
        folds
    }

    /// Whether renaming an entry between two spellings the filesystem folds
    /// together changes the listed name (case-insensitive APFS); elsewhere
    /// (case-sensitive APFS) it is a no-op and the old spelling stays.
    #[cfg(unix)]
    fn respells_in_place(dir: &Path) -> bool {
        let probe = dir.join("respell-caf\u{e9}");
        fs::write(&probe, b"").unwrap();
        let _ = fs::rename(&probe, dir.join("respell-cafe\u{301}"));
        let listed = fs::read_dir(dir)
            .unwrap()
            .any(|e| e.unwrap().file_name() == "respell-cafe\u{301}");
        for name in ["respell-caf\u{e9}", "respell-cafe\u{301}"] {
            let _ = fs::remove_file(dir.join(name));
        }
        listed
    }

    /// fb-661: renames between Unicode normalization forms, in both
    /// directions, of a file with new content, an unchanged executable, a
    /// symlink and a file under a renamed directory, whether the move rewrites
    /// the whole tree or deletes only the dropped paths. On a filesystem that
    /// folds normalization (APFS, the macOS default) each pair of spellings is
    /// one entry and deleting the dropped spelling deleted the target's file;
    /// it is renamed instead. Where the spellings are distinct the dropped
    /// files are deleted. Either way the tree, the modes and the stat cache
    /// end up exactly the target's, byte for byte under its spelling.
    #[cfg(unix)]
    #[test]
    fn apply_manifest_keeps_unicode_normalization_renames() {
        use std::os::unix::fs::PermissionsExt;
        // The two forms of the only accented letters used here.
        fn nfc(s: &str) -> String {
            s.replace("e\u{301}", "\u{e9}")
                .replace("i\u{308}", "\u{ef}")
        }
        fn nfd(s: &str) -> String {
            s.replace('\u{e9}', "e\u{301}")
                .replace('\u{ef}', "i\u{308}")
        }
        let files = |spell: fn(&str) -> String, new: bool| {
            vec![
                (
                    spell("caf\u{e9}.txt"),
                    if new { &b"y\n"[..] } else { &b"x\n"[..] },
                    FileMode::Regular,
                ),
                (
                    spell("na\u{ef}ve.sh"),
                    &b"#!/bin/sh\n"[..],
                    FileMode::Executable,
                ),
                (
                    spell("l\u{ef}nk"),
                    "caf\u{e9}.txt".as_bytes(),
                    FileMode::Symlink,
                ),
                (
                    spell("r\u{e9}sum\u{e9}/in.txt"),
                    &b"i\n"[..],
                    FileMode::Regular,
                ),
                ("plain.txt".to_string(), &b"p\n"[..], FileMode::Regular),
                // Case and normalization at once: no lowercasing pairs these.
                (
                    spell(if new {
                        "cr\u{e9}me.md"
                    } else {
                        "CR\u{e9}ME.md"
                    }),
                    &b"c\n"[..],
                    FileMode::Regular,
                ),
            ]
        };
        let probe = TempDir::new().unwrap();
        let folds = folds_unicode_normalization(probe.path());
        let respells = respells_in_place(probe.path());
        for (from, to) in [
            (nfc as fn(&str) -> String, nfd as fn(&str) -> String),
            (nfd, nfc),
        ] {
            for tracked_removed in [false, true] {
                let label = format!(
                    "{} -> {}, tracked_removed: {tracked_removed}, folds: {folds}, respells: {respells}",
                    if from("\u{e9}") == "\u{e9}" {
                        "NFC"
                    } else {
                        "NFD"
                    },
                    if to("\u{e9}") == "\u{e9}" {
                        "NFC"
                    } else {
                        "NFD"
                    },
                );
                let temp = TempDir::new().unwrap();
                let root = temp.path();
                let oak_dir = root.join(".oak");
                fs::create_dir(&oak_dir).unwrap();
                let lock = WorkdirLock::acquire(&oak_dir).unwrap();
                let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
                let manifest = |files: Vec<(String, &[u8], FileMode)>| {
                    Manifest::new(
                        files
                            .into_iter()
                            .map(|(path, content, mode)| ManifestEntry {
                                path,
                                blob_hash: repo.put_blob(content.to_vec()).unwrap(),
                                mode,
                            })
                            .collect(),
                    )
                };
                let old = manifest(files(from, false));
                let target = manifest(files(to, true));
                apply_manifest(&lock, root, &repo, &old, ApplyOpts::default()).unwrap();
                let delete = if tracked_removed {
                    DeleteScope::TrackedRemoved { old: &old }
                } else {
                    DeleteScope::Untracked
                };
                let opts = ApplyOpts {
                    delete,
                    ..ApplyOpts::default()
                };
                let report = apply_manifest(&lock, root, &repo, &target, opts).unwrap();

                // The names actually listed, byte for byte: the target's,
                // except where the filesystem folds the two spellings but
                // would not respell in place — then the old spelling stays
                // (with the target's bytes) and is reported, never deleted.
                let keeps = |old: String, new: String| {
                    if folds && !respells && old != new && nfc(&old) == nfc(&new) {
                        old
                    } else {
                        new
                    }
                };
                let mut expected: Vec<String> = files(from, false)
                    .into_iter()
                    .zip(files(to, true))
                    .map(|((old, _, _), (new, _, _))| keeps(old, new))
                    .collect();
                expected.push(keeps(from("r\u{e9}sum\u{e9}/"), to("r\u{e9}sum\u{e9}/")));
                expected.sort();
                assert_eq!(on_disk(root), expected, "{label}");
                let kept = report.kept_spellings.len();
                assert_eq!(kept > 0, folds && !respells, "{label}: {kept} kept");
                assert_eq!(fs::read(root.join(to("caf\u{e9}.txt"))).unwrap(), b"y\n");
                assert_eq!(
                    fs::read(root.join(to("r\u{e9}sum\u{e9}/in.txt"))).unwrap(),
                    b"i\n"
                );
                let exe = fs::symlink_metadata(root.join(to("na\u{ef}ve.sh"))).unwrap();
                assert_eq!(exe.permissions().mode() & 0o777, 0o755, "{label}");
                assert_eq!(
                    fs::read_link(root.join(to("l\u{ef}nk"))).unwrap(),
                    Path::new("caf\u{e9}.txt"),
                    "{label}"
                );
                // The stat cache describes the regular files as they now are
                // (a rename changes ctime), under the target's spelling only.
                let cache = repo.load_stat_cache().unwrap();
                let mut cached: Vec<&str> = cache.keys().map(String::as_str).collect();
                cached.sort_unstable();
                let mut regular: Vec<&str> = target
                    .entries
                    .iter()
                    .filter(|e| e.mode != FileMode::Symlink)
                    .map(|e| e.path.as_str())
                    .collect();
                regular.sort_unstable();
                assert_eq!(cached, regular, "{label}");
                for entry in target
                    .entries
                    .iter()
                    .filter(|e| e.mode != FileMode::Symlink)
                {
                    let row = &cache[&entry.path];
                    assert_eq!(row.blob_hash, entry.blob_hash);
                    let md = fs::metadata(root.join(&entry.path)).unwrap();
                    assert_eq!(
                        (row.size, Some(row.mtime_ns), row.ctime_ns),
                        crate::commands::commit::file_stat_for_cache(&md),
                        "{} {label}",
                        entry.path
                    );
                }
            }
        }
    }

    /// The identity check that keeps a renamed file never keeps a distinct
    /// one: a dropped path that is a second hard link to a target file, or a
    /// symlink to it, is its own entry and is deleted, while the target file
    /// survives.
    #[cfg(unix)]
    #[test]
    fn dropped_hard_links_and_symlinks_to_target_files_are_deleted() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let oak_dir = root.join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let entry = |path: &str, content: &[u8], mode| ManifestEntry {
            path: path.to_string(),
            blob_hash: repo.put_blob(content.to_vec()).unwrap(),
            mode,
        };
        let old = Manifest::new(vec![
            entry("kept.txt", b"k\n", FileMode::Regular),
            entry("hard.txt", b"k\n", FileMode::Regular),
            entry("soft", b"kept.txt", FileMode::Symlink),
        ]);
        let target = Manifest::new(vec![entry("kept.txt", b"k\n", FileMode::Regular)]);
        apply_manifest(&lock, root, &repo, &old, ApplyOpts::default()).unwrap();
        fs::remove_file(root.join("hard.txt")).unwrap();
        fs::hard_link(root.join("kept.txt"), root.join("hard.txt")).unwrap();
        let opts = ApplyOpts {
            delete: DeleteScope::TrackedRemoved { old: &old },
            ..ApplyOpts::default()
        };
        apply_manifest(&lock, root, &repo, &target, opts).unwrap();
        assert_eq!(on_disk(root), ["kept.txt"]);
        assert_eq!(fs::read(root.join("kept.txt")).unwrap(), b"k\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn apply_manifest_leaves_and_reports_non_utf8_deletion_candidate() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        let filename = OsString::from_vec(vec![0xff, b'.', b't', b'x', b't']);
        let path = temp.path().join(&filename);
        fs::write(&path, b"do not delete").unwrap();
        let blob = repo.put_blob(b"tracked\n".to_vec()).unwrap();
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "tracked.txt".to_string(),
            blob_hash: blob,
            mode: FileMode::Regular,
        }]);

        let report = apply_manifest(&lock, temp.path(), &repo, &manifest, ApplyOpts::default())
            .expect("non-UTF-8 deletion candidates should be left in place");

        assert_eq!(
            fs::read(temp.path().join("tracked.txt")).unwrap(),
            b"tracked\n",
            "materialization should still complete"
        );
        assert!(path.exists());
        assert_eq!(report.skipped_non_utf8_deletions.len(), 1);
    }
}
