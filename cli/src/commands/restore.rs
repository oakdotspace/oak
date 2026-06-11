use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use oak_core::{Hash, IgnorePatterns, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::materialize::{apply_manifest, ApplyOpts};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Restore working directory files to their HEAD state (or a specified source)
pub fn run(
    cwd: &Path,
    paths: &[std::path::PathBuf],
    source: Option<&str>,
    force: bool,
) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
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
                    files_to_delete.push(target_str.into_owned());
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

    if !force && !io::stdin().is_terminal() {
        let count = files_to_restore.len() + files_to_delete.len();
        return Err(OakError::DirtyWorkingTree(format!(
            "refusing to discard {count} change(s) without --force when not running interactively"
        )));
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
            output::info("Restore cancelled");
            return Ok(());
        }
    }

    // Materialize through the shared materializer: writes every matching
    // entry, deletes untracked files (matching the path filter), prunes
    // empty dirs on a whole-tree restore, refreshes the stat cache.
    apply_manifest(
        &lock,
        root,
        &repo,
        &manifest,
        ApplyOpts {
            filter: if restore_all {
                None
            } else {
                Some(&matches_target as &dyn Fn(&str) -> bool)
            },
            clean_empty_dirs: restore_all,
            ..ApplyOpts::default()
        },
    )?;

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

        // Classify without following symlinks: a symlink to a directory
        // (even one outside the repo) is a single untracked entry, never a
        // directory to recurse into — descending through it would list (and
        // later delete) the *target's* contents.
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
