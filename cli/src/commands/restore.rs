use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use oak_core::{FileMode, Hash, IgnorePatterns, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Restore working directory files to their HEAD state (or a specified source)
pub fn run(
    cwd: &Path,
    paths: &[std::path::PathBuf],
    source: Option<&str>,
    force: bool,
) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let _lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = ctx.work_tree.clone();
    let root = root.as_path();
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    // Resolve the source commit
    let (commit_hash, commit) = if let Some(src) = source {
        let hash = Hash::from_hex(src).map_err(|_| OakError::CommitNotFound(src.to_string()))?;
        let c = repo
            .get_commit(&hash)?
            .ok_or_else(|| OakError::CommitNotFound(src.to_string()))?;
        (hash, c)
    } else {
        // Default to HEAD
        let branch_name = repo.get_current_branch_name().ok().flatten();
        let head = if let Some(ref name) = branch_name {
            repo.get_branch_head(name)?
        } else {
            repo.get_head()?
        };

        let head = match head {
            Some(h) => h,
            None => {
                output::info("No commits yet, nothing to restore from");
                return Ok(());
            }
        };

        let c = repo
            .get_commit(&head)?
            .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
        (head, c)
    };

    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;

    // When a project scope is active, narrow the manifest so restore only
    // considers (and only ever writes/deletes) in-scope paths. Out-of-
    // scope files are not part of the working tree under that scope and
    // shouldn't be touched.
    let prefixes = super::project::active_prefixes(&repo)?;
    let manifest = if prefixes.is_empty() {
        manifest
    } else {
        oak_core::filter_manifest_by_prefixes(&manifest, &prefixes)
    };

    let ignore = IgnorePatterns::new(root)?;

    // If no paths given, restore everything (like git restore .)
    let restore_all = paths.is_empty();

    // Normalize target paths to be repo-root-relative. User-supplied
    // relative paths are interpreted as cwd-relative (matching git's
    // behavior when invoked from a subdirectory of the repo).
    let target_relatives: Vec<std::path::PathBuf> = paths
        .iter()
        .map(|p| crate::pathutil::repo_relative(cwd, root, p))
        .collect();

    let matches_target = |file_path: &str| -> bool {
        if restore_all {
            return true;
        }

        for target in &target_relatives {
            let target_str = target.to_string_lossy();
            if file_path == target_str.as_ref() || file_path.starts_with(&format!("{target_str}/"))
            {
                return true;
            }
        }
        false
    };

    // Find all changes that would be discarded
    let head_files: HashSet<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    let mut files_to_restore: Vec<String> = Vec::new();
    let mut files_to_delete: Vec<String> = Vec::new();

    // Check for modified/deleted tracked files
    for entry in &manifest.entries {
        if !matches_target(&entry.path) {
            continue;
        }

        let file_path = root.join(&entry.path);

        if !file_path.exists() {
            files_to_restore.push(format!("  restore: {}", entry.path));
        } else {
            let content = fs::read(&file_path)?;
            let current_hash = oak_core::hash_bytes(&content);
            // Compare mode too, not just content: a chmod-only change leaves
            // the blob hash identical but still differs from the source. Mirror
            // `oak status` (Manifest::diff compares mode) so restore can undo a
            // mode-only change instead of reporting the tree clean.
            let mode_changed = crate::file_permissions::current_file_mode(&file_path)
                .is_some_and(|m| m != entry.mode);
            if current_hash != entry.blob_hash || mode_changed {
                files_to_restore.push(format!("  revert:  {}", entry.path));
            }
        }
    }

    // Check for untracked files to delete
    if restore_all {
        find_new_files(
            root,
            root,
            &ignore,
            &head_files,
            &mut files_to_delete,
            &matches_target,
        )?;
    } else {
        for target in &target_relatives {
            let full_target = root.join(target);
            if full_target.is_dir() {
                find_new_files(
                    &full_target,
                    root,
                    &ignore,
                    &head_files,
                    &mut files_to_delete,
                    &matches_target,
                )?;
            } else if full_target.is_file() {
                let target_str = target.to_string_lossy();
                if !head_files.contains(target_str.as_ref()) {
                    files_to_delete.push(format!("  delete:  {target_str}"));
                }
            }
        }
    }

    if files_to_restore.is_empty() && files_to_delete.is_empty() {
        if restore_all {
            output::info("Working directory is clean, nothing to restore");
        } else {
            output::info("Specified paths are already at source state, nothing to restore");
        }
        return Ok(());
    }

    println!("The following changes will be discarded:\n");

    if !files_to_restore.is_empty() {
        println!("Tracked files:");
        for file in &files_to_restore {
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
            output::info("Restore cancelled");
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

    if restore_all {
        clean_empty_dirs(root, &ignore)?;
    }

    if source.is_some() {
        output::success(&format!("Restored to {}", &commit_hash.to_string()[..12]));
    } else if restore_all {
        output::success(&format!(
            "Restored to HEAD ({})",
            &commit_hash.to_string()[..12]
        ));
    } else {
        let path_list: Vec<String> = paths.iter().map(|p| format!("'{}'", p.display())).collect();
        output::success(&format!("Restored {}", path_list.join(", ")));
    }

    Ok(())
}

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
