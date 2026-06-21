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
use std::path::Path;

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
            clean_empty_dirs: true,
        }
    }
}

#[derive(Default)]
pub struct ApplyReport {
    /// Paths skipped because their blob is missing (`MissingBlobs::Skip`).
    pub skipped: Vec<String>,
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
    let matches = |p: &str| opts.filter.is_none_or(|f| f(p));

    let mut report = ApplyReport::default();

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
                    continue;
                }
                MissingBlobs::Error => {
                    return Err(OakError::BlobNotFound(format!(
                        "{} for '{}'",
                        entry.blob_hash, entry.path
                    )));
                }
            },
        };
        materialize_path(&file_path, entry.mode, &blob.content)?;
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

    let target_paths: HashSet<&str> = target.entries.iter().map(|e| e.path.as_str()).collect();
    match opts.delete {
        DeleteScope::Untracked => {
            activity.set_message("Removing files no longer tracked...");
            delete_untracked(
                root,
                root,
                &ignore,
                &target_paths,
                &matches,
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
    let full_mirror = matches!(opts.delete, DeleteScope::Untracked) && opts.filter.is_none();
    if full_mirror {
        crate::commands::commit::refresh_stat_cache_after_materialize(repo, cache_upserts)?;
    } else {
        repo.update_stat_cache(&cache_upserts, &report.deleted)?;
    }

    Ok(report)
}

/// Write one materialized entry to disk, replacing whatever is currently at
/// `file_path`. Symlink entries become real symlinks — their stored content is
/// the target path; regular/executable entries are written as files. Any
/// existing file, symlink, or directory at the path is removed first so we never
/// write *through* an existing symlink (which would follow it into its target —
/// the `EISDIR` crash a git import with directory symlinks like git/git's
/// `subprojects/gitk -> ../gitk-git` hits) and so file<->symlink<->dir
/// transitions all work.
pub(crate) fn materialize_path(file_path: &Path, mode: FileMode, content: &[u8]) -> Result<()> {
    match fs::symlink_metadata(file_path) {
        Ok(md) if md.file_type().is_dir() => fs::remove_dir_all(file_path)?,
        Ok(_) => fs::remove_file(file_path)?,
        Err(_) => {}
    }
    if mode == FileMode::Symlink {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(content);
            std::os::unix::fs::symlink(target, file_path)?;
        }
        #[cfg(not(unix))]
        {
            // No portable symlink-to-anything on Windows; preserve the data as a
            // regular file holding the target path rather than failing the
            // checkout. (oak's shipped mount/Windows story is separate.)
            fs::write(file_path, content)?;
        }
    } else {
        fs::write(file_path, content)?;
        crate::file_permissions::apply_file_permissions(file_path, mode)?;
    }
    Ok(())
}

/// Delete files not in the manifest (and matching the caller's filter),
/// recording what was removed.
fn delete_untracked(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
    target_paths: &HashSet<&str>,
    matches: &dyn Fn(&str) -> bool,
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
        if is_real_dir {
            delete_untracked(&path, root, ignore, target_paths, matches, deleted)?;
        } else {
            let rel_str = relative.to_string_lossy();
            if !target_paths.contains(rel_str.as_ref()) && matches(&rel_str) {
                fs::remove_file(&path)?;
                deleted.push(rel_str.into_owned());
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
}
