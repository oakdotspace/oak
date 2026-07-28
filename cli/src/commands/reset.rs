use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use oak_core::{IgnorePatterns, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::materialize::{apply_manifest, ApplyOpts};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Reset working directory (or a specific path) to HEAD
pub fn run(cwd: &Path, target_path: Option<&Path>, force: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = ctx.work_tree.clone();
    let root = root.as_path();
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    // Get current HEAD: prefer DB branch, fallback to legacy head
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = if let Some(ref name) = branch_name {
        repo.get_branch_head(name)?
    } else {
        repo.get_head()?
    };

    let head = match head {
        Some(h) => h,
        None => {
            output::info("No commits yet, nothing to reset to");
            return Ok(());
        }
    };

    // Get the manifest for HEAD commit
    let commit = repo
        .get_commit(&head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;

    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;

    let ignore = IgnorePatterns::new(root)?;

    // Normalize target path to be relative to repo root. User-supplied
    // relative paths are interpreted as cwd-relative (matching git's
    // behavior when the user is inside a subdirectory).
    let target_relative = target_path
        .map(|p| crate::pathutil::repo_relative_str(cwd, root, &p.to_string_lossy()))
        .transpose()?;

    // Find all changes that would be discarded
    let mut files_to_reset: Vec<String> = Vec::new();
    let mut files_to_delete: Vec<String> = Vec::new();

    // Build set of files in HEAD
    let head_files: HashSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();

    // Helper to check if a path matches the target
    let matches_target = |file_path: &str| -> bool {
        match &target_relative {
            None => true,
            Some(target) => super::diff::path_matches(file_path, std::slice::from_ref(target)),
        }
    };

    // Check for modified/deleted tracked files
    for entry in &manifest.entries {
        if !matches_target(&entry.path) {
            continue;
        }

        let file_path = root.join(&entry.path);

        if !file_path.exists() {
            files_to_reset.push(format!("  restore: {}", entry.path));
        } else {
            let content = fs::read(&file_path)?;
            let current_hash = oak_core::hash_bytes(&content);
            // Compare mode too, not just content: a chmod-only change (e.g.
            // `chmod +x`) leaves the blob hash identical but still differs from
            // HEAD. `oak status` flags it (Manifest::diff compares mode), so
            // reset must as well — otherwise the two disagree and reset can't
            // undo a mode-only change.
            let mode_changed = crate::file_permissions::current_file_mode(&file_path)
                .is_some_and(|m| m != entry.mode);
            if current_hash != entry.blob_hash || mode_changed {
                files_to_reset.push(format!("  revert:  {}", entry.path));
            }
        }
    }

    // Check for untracked files
    fn find_new_files(
        dir: &Path,
        root: &Path,
        ignore: &IgnorePatterns,
        head_files: &HashSet<&str>,
        new_files: &mut Vec<String>,
        matches_target: &dyn Fn(&str) -> bool,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).unwrap();

            // Classify without following symlinks: a symlink to a directory
            // (even one outside the repo) is a single untracked entry, never
            // a directory to recurse into — descending through it would list
            // (and later delete) the *target's* contents.
            let file_type = entry.file_type()?;
            let is_real_dir = file_type.is_dir();
            if ignore.is_ignored(relative, is_real_dir) {
                continue;
            }

            if is_real_dir {
                find_new_files(
                    &entry_path,
                    root,
                    ignore,
                    head_files,
                    new_files,
                    matches_target,
                )?;
            } else {
                let relative_str = relative.to_string_lossy();
                if !head_files.contains(relative_str.as_ref()) && matches_target(&relative_str) {
                    new_files.push(relative_str.into_owned());
                }
            }
        }
        Ok(())
    }

    let search_dir = match &target_relative {
        Some(target) => {
            let full_target = root.join(Path::new(target));
            if full_target.is_dir() {
                full_target
            } else if full_target.is_file() {
                if !head_files.contains(target.as_str()) {
                    files_to_delete.push(target.clone());
                }
                root.to_path_buf()
            } else {
                root.to_path_buf()
            }
        }
        None => root.to_path_buf(),
    };

    if target_relative.is_none()
        || search_dir != *root
        || target_relative
            .as_ref()
            .map(|t| root.join(Path::new(t)).is_dir())
            .unwrap_or(false)
    {
        let search_start = if target_relative.is_some() && search_dir != *root {
            &search_dir
        } else {
            root
        };
        find_new_files(
            search_start,
            root,
            &ignore,
            &head_files,
            &mut files_to_delete,
            &matches_target,
        )?;
    }

    // Tracked paths that ignore rules now cover are in a state reset cannot
    // clear: they're on disk, but the status scan skips them, so `oak status`
    // reports them as pending deletions no matter what reset writes. Detect
    // them through the same predicate status's warning uses, and explain —
    // never claim "already at HEAD state" while status disagrees (fb-25).
    let now_ignored: Vec<String> = manifest
        .entries
        .iter()
        .filter(|e| matches_target(&e.path))
        .filter(|e| super::commit::is_tracked_path_now_ignored(&e.path, root, &ignore))
        .map(|e| e.path.clone())
        .collect();

    if files_to_reset.is_empty() && files_to_delete.is_empty() {
        if !now_ignored.is_empty() {
            super::commit::explain_now_ignored_unclearable(&now_ignored, "reset");
        } else if target_relative.is_some() {
            output::info("Path is already at HEAD state, nothing to reset");
        } else {
            output::info("Working directory is clean, nothing to reset");
        }
        return Ok(());
    }

    if !force && !io::stdin().is_terminal() {
        let count = files_to_reset.len() + files_to_delete.len();
        return Err(OakError::DirtyWorkingTree(format!(
            "refusing to discard {count} change(s) without --force when not running interactively"
        )));
    }

    println!("The following changes will be discarded:\n");

    if !files_to_reset.is_empty() {
        println!("Tracked files:");
        for file in &files_to_reset {
            println!("{file}");
        }
        println!();
    }

    if !files_to_delete.is_empty() {
        println!("Untracked files to delete:");
        for file in &files_to_delete {
            println!("  delete:  {file}");
        }
        println!();
    }

    if !force {
        print!("Are you sure you want to discard these changes? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            output::info("Reset cancelled");
            return Ok(());
        }
    }

    // Materialize through the shared materializer: writes every matching
    // entry, deletes untracked files (matching the target filter), prunes
    // empty dirs on a whole-tree reset, refreshes the stat cache.
    apply_manifest(
        &lock,
        root,
        &repo,
        &manifest,
        ApplyOpts {
            filter: target_relative
                .as_ref()
                .map(|_| &matches_target as &dyn Fn(&str) -> bool),
            clean_empty_dirs: target_relative.is_none(),
            ..ApplyOpts::default()
        },
    )?;

    if let Some(target) = target_path {
        output::success(&format!("Reset '{}'", target.display()));
    } else {
        output::success(&format!("Reset to {}", &commit.hash.to_string()[..12]));
    }

    // Even after a successful reset, now-ignored tracked paths keep showing
    // as pending deletions in `oak status` — say why, so the lingering report
    // is never a surprise. Re-evaluate with fresh ignore rules: the reset may
    // have deleted an untracked .oakignore/.gitignore, un-ignoring the paths.
    if !now_ignored.is_empty() {
        let ignore_after = IgnorePatterns::new(root)?;
        let still_ignored: Vec<String> = now_ignored
            .into_iter()
            .filter(|p| super::commit::is_tracked_path_now_ignored(p, root, &ignore_after))
            .collect();
        super::commit::explain_now_ignored_unclearable(&still_ignored, "reset");
    }

    Ok(())
}

/// Reset working directory to fully match a specific manifest: write every
/// entry, delete untracked files, prune empty dirs, refresh the stat cache.
/// Thin wrapper over [`crate::materialize::apply_manifest`] — the single
/// shared materializer.
pub fn reset_to_manifest(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    manifest: &oak_core::Manifest,
) -> Result<()> {
    apply_manifest(lock, root, repo, manifest, ApplyOpts::default()).map(|_| ())
}
