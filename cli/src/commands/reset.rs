use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use oak_core::{FileMode, IgnorePatterns, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Reset working directory (or a specific path) to HEAD
pub fn run(cwd: &Path, target_path: Option<&Path>, force: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let _lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
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
    let target_relative = target_path.map(|p| crate::pathutil::repo_relative(cwd, root, p));

    // Find all changes that would be discarded
    let mut files_to_reset: Vec<String> = Vec::new();
    let mut files_to_delete: Vec<String> = Vec::new();

    // Build set of files in HEAD
    let head_files: HashSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();

    // Helper to check if a path matches the target
    let matches_target = |file_path: &str| -> bool {
        match &target_relative {
            None => true,
            Some(target) => {
                let target_str = target.to_string_lossy();
                file_path == target_str.as_ref() || file_path.starts_with(&format!("{target_str}/"))
            }
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

            if ignore.is_ignored(relative, entry_path.is_dir()) {
                continue;
            }

            if entry_path.is_dir() {
                find_new_files(
                    &entry_path,
                    root,
                    ignore,
                    head_files,
                    new_files,
                    matches_target,
                )?;
            } else if entry_path.is_file() {
                let relative_str = relative.to_string_lossy();
                if !head_files.contains(relative_str.as_ref()) && matches_target(&relative_str) {
                    new_files.push(format!("  delete:  {relative_str}"));
                }
            }
        }
        Ok(())
    }

    let search_dir = match &target_relative {
        Some(target) => {
            let full_target = root.join(target);
            if full_target.is_dir() {
                full_target
            } else if full_target.is_file() {
                let target_str = target.to_string_lossy();
                if !head_files.contains(target_str.as_ref()) {
                    files_to_delete.push(format!("  delete:  {target_str}"));
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
            .map(|t| root.join(t).is_dir())
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

    if files_to_reset.is_empty() && files_to_delete.is_empty() {
        if target_relative.is_some() {
            output::info("Path is already at HEAD state, nothing to reset");
        } else {
            output::info("Working directory is clean, nothing to reset");
        }
        return Ok(());
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
            println!("{file}");
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

    for entry in &manifest.entries {
        if !matches_target(&entry.path) {
            continue;
        }

        let file_path = root.join(&entry.path);

        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let blob = repo
            .get_blob(&entry.blob_hash)?
            .ok_or_else(|| OakError::BlobNotFound(entry.blob_hash.to_string()))?;

        // Remove existing file/symlink before writing
        if file_path.exists() || file_path.is_symlink() {
            fs::remove_file(&file_path)?;
        }

        if entry.mode == FileMode::Symlink {
            let target = String::from_utf8_lossy(&blob.content);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target.as_ref(), &file_path)?;
            #[cfg(not(unix))]
            fs::write(&file_path, &blob.content)?;
        } else {
            fs::write(&file_path, &blob.content)?;
        }
        crate::file_permissions::apply_file_permissions(&file_path, entry.mode)?;
    }

    for entry in &files_to_delete {
        let file_path_str = entry.trim_start_matches("  delete:  ");
        let file_path = root.join(file_path_str);
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
    }

    if target_relative.is_none() {
        clean_empty_dirs(root, &ignore)?;
    }

    if let Some(target) = target_path {
        output::success(&format!("Reset '{}'", target.display()));
    } else {
        output::success(&format!("Reset to {}", &commit.hash.to_string()[..12]));
    }

    Ok(())
}

/// Reset working directory to match a specific manifest
pub fn reset_to_manifest(
    root: &Path,
    repo: &dyn Repository,
    manifest: &oak_core::Manifest,
    ignore: &IgnorePatterns,
) -> Result<()> {
    // Restore all files from manifest. Collect a fresh stat-cache row for
    // every regular file we write, so the cache mirrors the tree we just
    // materialized rather than a stale row from another branch's version of
    // the path (see `stat_cache_upsert`). Symlinks are never cached.
    let mut cache_upserts = Vec::new();
    for entry in &manifest.entries {
        let file_path = root.join(&entry.path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let blob = repo
            .get_blob(&entry.blob_hash)?
            .ok_or_else(|| OakError::BlobNotFound(entry.blob_hash.to_string()))?;

        // Remove existing file/symlink before writing
        if file_path.exists() || file_path.is_symlink() {
            fs::remove_file(&file_path)?;
        }

        if entry.mode == FileMode::Symlink {
            let target = String::from_utf8_lossy(&blob.content);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target.as_ref(), &file_path)?;
            #[cfg(not(unix))]
            fs::write(&file_path, &blob.content)?;
        } else {
            fs::write(&file_path, &blob.content)?;
        }
        crate::file_permissions::apply_file_permissions(&file_path, entry.mode)?;

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

    // Delete files not in manifest
    let head_files: HashSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    delete_untracked(root, root, ignore, &head_files)?;
    clean_empty_dirs(root, ignore)?;

    // `reset_to_manifest` rewrites the whole tree (no scope), so the cache
    // should now equal the manifest: keep the rows we wrote and drop any
    // left over for paths no longer present.
    crate::commands::commit::refresh_stat_cache_after_materialize(repo, cache_upserts, &[])?;

    Ok(())
}

/// Delete files not in the manifest
fn delete_untracked(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
    head_files: &HashSet<&str>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();
        if ignore.is_ignored(relative, path.is_dir()) {
            continue;
        }
        if path.is_dir() {
            delete_untracked(&path, root, ignore, head_files)?;
        } else if path.is_file() {
            let rel_str = relative.to_string_lossy();
            if !head_files.contains(rel_str.as_ref()) {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// Remove empty directories (except .oak)
fn clean_empty_dirs(path: &Path, ignore: &IgnorePatterns) -> Result<()> {
    fn clean_recursive(dir: &Path, root: &Path, ignore: &IgnorePatterns) -> Result<bool> {
        let mut is_empty = true;

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let relative = entry_path.strip_prefix(root).unwrap();

            if ignore.is_ignored(relative, entry_path.is_dir()) {
                is_empty = false;
                continue;
            }

            if entry_path.is_dir() {
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
