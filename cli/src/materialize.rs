//! The one materializer: make the working directory match a manifest.
//!
//! `oak switch`, `oak reset`, `oak restore`, `oak pull`, and the merge/sync
//! conflict write-out all used to carry their own copy of "write this manifest
//! to disk / delete what shouldn't be there", and each copy had bugs the
//! others had fixed (symlink-following deletion, missing stat-cache refresh,
//! never-deleting pulls). They all route through [`apply_manifest`] now;
//! behavior differences between callers are expressed as [`ApplyOpts`], not
//! forks.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{FileMode, IgnorePatterns, Manifest, OakError, Repository, Result};

use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Which on-disk files [`apply_manifest`] deletes.
pub enum DeleteScope<'a> {
    /// Delete every file (matching the filter) that isn't in the target
    /// manifest — full materialization (switch, reset, restore).
    Untracked,
    /// Delete only paths tracked in `old` that are absent from the target —
    /// a fast-forward (pull) that must preserve the user's untracked files
    /// while still applying upstream deletions.
    TrackedRemoved { old: &'a Manifest },
    /// Delete nothing (merge/sync conflict write-out).
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
    /// Paths deleted from the working tree.
    pub deleted: Vec<String>,
}

/// Make the working dir match `target`. Writes every (matching) manifest
/// entry, deletes per [`DeleteScope`], prunes emptied directories, and updates
/// the stat cache. Classification never follows symlinks (lstat), so a
/// working-tree symlink pointing at a directory — even one outside the repo —
/// is treated as a single entry, never recursed into.
///
/// The `&WorkdirLock` witness is deliberately unused: requiring it makes
/// unlocked working-tree mutation a compile error rather than a race.
pub fn apply_manifest(
    _lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
    opts: ApplyOpts<'_>,
) -> Result<ApplyReport> {
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
    let replaceable_paths = replaceable_directory_paths(repo, &opts)?;

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
        let mut missing_blobs = Vec::new();
        for entry in &target.entries {
            if matches(&entry.path) && repo.get_blob(&entry.blob_hash)?.is_none() {
                missing_blobs.push((entry.blob_hash.clone(), entry.path.clone()));
            }
        }
        if !missing_blobs.is_empty() {
            return Err(OakError::IncompleteBlobData {
                context: format_missing_blob_context(&missing_blobs),
                missing: format_missing_blob_hashes(&missing_blobs),
            });
        }
    }

    // Write every matching entry, collecting a fresh stat-cache row per
    // regular file. The cache is keyed by path only and shared across every
    // branch in the working dir, so a row left over from another branch's
    // version of a path would otherwise be trusted on the next scan and
    // silently record a foreign blob. Symlinks are never cached.
    let mut cache_upserts = Vec::new();
    for entry in &target.entries {
        if !matches(&entry.path) {
            continue;
        }
        let file_path = root.join(&entry.path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
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
        materialize_path_for_apply(
            root,
            &file_path,
            entry.mode,
            &blob.content,
            &replaceable_paths,
        )?;
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

    match opts.delete {
        DeleteScope::Untracked => {
            activity.set_message("Removing files no longer tracked...");
            delete_untracked(
                root,
                root,
                &ignore,
                &target_paths,
                &matches,
                &mut report.skipped_non_utf8_deletions,
                &mut report.deleted,
            )?;
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
                let file_path = root.join(&entry.path);
                match fs::symlink_metadata(&file_path) {
                    // lstat: delete the file or symlink itself, never what a
                    // symlink points at.
                    Ok(md) if !md.is_dir() => fs::remove_file(&file_path)?,
                    // Replaced by a real directory, or already gone — leave
                    // it for the next scan to report.
                    _ => {}
                }
                // Prune the cache row either way: the tracked file this row
                // described no longer exists at this path.
                report.deleted.push(entry.path.clone());
            }
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

    Ok(report)
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
pub(crate) fn materialize_path(file_path: &Path, mode: FileMode, content: &[u8]) -> Result<()> {
    materialize_path_inner(file_path, mode, content, None)
}

fn materialize_path_for_apply(
    root: &Path,
    file_path: &Path,
    mode: FileMode,
    content: &[u8],
    replaceable_paths: &HashSet<String>,
) -> Result<()> {
    materialize_path_inner(file_path, mode, content, Some((root, replaceable_paths)))
}

fn materialize_path_inner(
    file_path: &Path,
    mode: FileMode,
    content: &[u8],
    replaceable_dir_context: Option<(&Path, &HashSet<String>)>,
) -> Result<()> {
    match fs::symlink_metadata(file_path) {
        Ok(md) if md.file_type().is_dir() => {
            if dir_has_entries(file_path)? {
                let can_replace = match replaceable_dir_context {
                    Some((root, replaceable_paths)) => directory_contains_only_replaceable_paths(
                        file_path,
                        root,
                        replaceable_paths,
                    )?,
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
            let tmp_path = temp_sibling(file_path)?;
            let write_result = (|| -> Result<()> {
                std::os::unix::fs::symlink(target, &tmp_path)?;
                fs::rename(&tmp_path, file_path)?;
                Ok(())
            })();
            if write_result.is_err() {
                let _ = fs::remove_file(&tmp_path);
            }
            write_result?;
        }
        #[cfg(not(unix))]
        {
            // No portable symlink-to-anything on Windows; preserve the data as a
            // regular file holding the target path rather than failing the
            // checkout. (oak's shipped mount/Windows story is separate.)
            crate::atomic_file::write_atomic(file_path, content)?;
        }
    } else {
        crate::atomic_file::write_atomic(file_path, content)?;
        crate::file_permissions::apply_file_permissions(file_path, mode)?;
    }
    Ok(())
}

fn replaceable_directory_paths(
    repo: &dyn Repository,
    opts: &ApplyOpts<'_>,
) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();
    if let Some(head) = repo.get_head()? {
        if let Some(commit) = repo.get_commit(&head)? {
            if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
                paths.extend(manifest.entries.into_iter().map(|entry| entry.path));
            }
        }
    }
    if let DeleteScope::TrackedRemoved { old } = &opts.delete {
        paths.extend(old.entries.iter().map(|entry| entry.path.clone()));
    }
    Ok(paths)
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
        let Some(rel_str) = relative.to_str() else {
            return Err(OakError::DirtyWorkingTree(format!(
                "refusing to replace directory containing non-UTF-8 path '{}'",
                relative.display()
            )));
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

#[cfg(unix)]
fn temp_sibling(path: &Path) -> Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        OakError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "materialize path has no file name",
        ))
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    Ok(path.with_file_name(format!(
        ".{}.oak-tmp-{}-{nonce}",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}

/// Delete files not in the manifest (and matching the caller's filter),
/// recording what was removed.
fn delete_untracked(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
    target_paths: &HashSet<&str>,
    matches: &dyn Fn(&str) -> bool,
    skipped_non_utf8: &mut Vec<String>,
    deleted: &mut Vec<String>,
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
            skipped_non_utf8.push(display);
            continue;
        };
        if is_real_dir {
            delete_untracked(
                &path,
                root,
                ignore,
                target_paths,
                matches,
                skipped_non_utf8,
                deleted,
            )?;
        } else {
            if !target_paths.contains(rel_str) && matches(rel_str) {
                fs::remove_file(&path)?;
                deleted.push(rel_str.to_string());
            }
        }
    }
    Ok(())
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
            if ignore.is_ignored(rel_dir, true) {
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
