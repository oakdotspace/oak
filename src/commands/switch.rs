use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use dialoguer::{Confirm, Select};
use oak_core::{Hash, IgnorePatterns, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::commands::commit::get_status;
use crate::output;
use crate::resolve::Backend;

/// Switch to a branch or detach HEAD at a specific commit. When `name` is
/// `None` (no argument supplied), prompt the user to pick a branch
/// interactively.
pub fn run(path: &Path, name: Option<&str>, detach: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    // Check for uncommitted changes first
    let (changes, _, _) = get_status(path)?;
    if !changes.is_empty() {
        return Err(OakError::UncommittedChanges);
    }

    let name = match name {
        Some(n) => n.to_string(),
        None => {
            if detach {
                return Err(OakError::Io(std::io::Error::other(
                    "--detach requires a commit hash",
                )));
            }
            select_branch_interactive(repo.as_ref())?
        }
    };
    let name = name.as_str();

    // `main` only exists on the server. Locally users always work on a
    // feature/personal branch, so reject any attempt to switch to it.
    if !detach && name == "main" {
        return Err(OakError::Io(std::io::Error::other(
            "`main` only exists on the server — work on a personal/feature branch and let the server squash-merge your branch description onto main",
        )));
    }

    if detach {
        // --detach: treat name as a commit hash
        let hash = Hash::from_hex(name)?;
        let commit = repo
            .get_commit(&hash)?
            .ok_or_else(|| OakError::CommitNotFound(hash.to_string()))?;
        let manifest = repo
            .get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(oak_core::Manifest::empty);

        update_working_dir(root, repo.as_ref(), &manifest)?;

        repo.set_current_branch("")?;
        repo.set_head(&hash)?;

        output::success(&format!("HEAD is now at {} (detached)", hash.short()));
        if let Some(msg) = commit.message.as_deref() {
            output::info(&format!("  {msg}"));
        }
        return Ok(());
    }

    // Try to find as a branch
    if let Some(branch) = repo.get_branch(name)? {
        let head_hash = repo.get_branch_head(&branch.name)?;
        let manifest = if let Some(ref h) = head_hash {
            let commit = repo
                .get_commit(h)?
                .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
            repo.get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(oak_core::Manifest::empty)
        } else {
            oak_core::Manifest::empty()
        };

        update_working_dir(root, repo.as_ref(), &manifest)?;

        repo.set_current_branch(&branch.name)?;
        if let Some(ref h) = head_hash {
            repo.set_head(h)?;
        }

        output::success(&format!("Switched to branch '{}'", branch.name));
        return Ok(());
    }

    // Try to treat as a commit hash (detach automatically)
    let looks_like_hash = Hash::from_hex(name).is_ok();
    if let Ok(hash) = Hash::from_hex(name) {
        if let Some(commit) = repo.get_commit(&hash)? {
            let manifest = repo
                .get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(oak_core::Manifest::empty);

            update_working_dir(root, repo.as_ref(), &manifest)?;

            repo.set_current_branch("")?;
            repo.set_head(&hash)?;

            output::success(&format!("HEAD is now at {} (detached)", hash.short()));
            if let Some(msg) = commit.message.as_deref() {
                output::info(&format!("  {msg}"));
            }
            return Ok(());
        }
    }

    // Branch doesn't exist. If we're attached to a TTY, offer to create it
    // off either the current branch or the server's main branch. A
    // hash-shaped name is almost certainly a typo'd commit ref, not a new
    // branch — skip the prompt in that case.
    if !looks_like_hash && is_interactive() {
        // Drop the borrow on `repo` before re-opening through `branch::new_branch`.
        drop(repo);
        return prompt_create_and_switch(path, &ctx, name);
    }

    Err(OakError::BranchNotFound(format!(
        "'{name}' is not a branch or commit hash"
    )))
}

/// Create a branch and switch to it. This is the implementation behind
/// `oak switch -c <name>`.
pub fn create(path: &Path, name: &str) -> Result<()> {
    super::branch::new_branch(path, name, None, None, None)
}

fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Prompt the user to create the missing branch and pick its parent
/// (current branch vs server's `main`). Used by `oak switch <name>` when
/// `<name>` doesn't exist yet.
fn prompt_create_and_switch(
    path: &Path,
    ctx: &crate::resolve::RepoContext,
    name: &str,
) -> Result<()> {
    let create = Confirm::new()
        .with_prompt(format!("Branch '{name}' doesn't exist. Create it?"))
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    if !create {
        return Err(OakError::BranchNotFound(format!(
            "'{name}' is not a branch or commit hash"
        )));
    }

    // Re-open the repo to read current branch + remote metadata. Cheap; the
    // db handle is just an in-process file lock.
    let repo = ctx.open()?;
    let current = repo.get_current_branch_name().ok().flatten();
    let has_remote = repo
        .get_metadata(MetadataKey::RemoteUrl)
        .ok()
        .flatten()
        .is_some();
    // Only the SQLite backend can materialize main from the server today —
    // that path needs a `SqliteRepository` to write commit/manifest/blob rows.
    let can_fetch_main = has_remote && matches!(ctx.backend, Backend::Sqlite);
    drop(repo);

    enum ParentKind {
        Current,
        ServerMain,
    }

    let mut options: Vec<String> = Vec::new();
    let mut kinds: Vec<ParentKind> = Vec::new();
    if let Some(ref c) = current {
        options.push(format!("Current branch ('{c}')"));
        kinds.push(ParentKind::Current);
    }
    if can_fetch_main {
        options.push("Server's main branch".to_string());
        kinds.push(ParentKind::ServerMain);
    }

    if options.is_empty() {
        return Err(OakError::Io(std::io::Error::other(
            "no parent available — not on a branch and no remote configured",
        )));
    }

    let idx = if options.len() == 1 {
        0
    } else {
        Select::new()
            .with_prompt("Branch off of?")
            .items(&options)
            .default(0)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?
    };

    match kinds[idx] {
        ParentKind::Current => super::branch::new_branch(path, name, None, None, None),
        ParentKind::ServerMain => {
            // Fetch main's head + manifest + missing blobs into local
            // storage, then create the branch parented onto `main` and
            // seeded at that head — `branch::new_branch` will rewrite the
            // working tree to match.
            let db_path = ctx.db_path()?;
            let sqlite_repo = SqliteRepository::open(&db_path)?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            let head = rt
                .block_on(super::sync::fetch_parent_from_server(&sqlite_repo, "main"))?
                .ok_or_else(|| OakError::Server("Server has no commits on main yet".to_string()))?;
            drop(sqlite_repo);
            super::branch::new_branch(path, name, None, Some(head.as_str()), Some("main"))
        }
    }
}

/// Prompt the user to pick a branch with `dialoguer::Select`. Used when
/// `oak switch` is invoked without a name argument.
fn select_branch_interactive(repo: &dyn Repository) -> Result<String> {
    let branches = repo.list_branches()?;
    if branches.is_empty() {
        return Err(OakError::Io(std::io::Error::other(
            "no branches in this repository",
        )));
    }
    let current = repo.get_current_branch_name().ok().flatten();

    let mut items = Vec::with_capacity(branches.len());
    let mut default_idx = 0;
    for (i, br) in branches.iter().enumerate() {
        let is_current = current.as_deref() == Some(&br.name);
        if is_current {
            default_idx = i;
        }
        let marker = if is_current { "* " } else { "  " };
        let status = br.status.as_str();
        let head_short = repo
            .get_branch_head(&br.name)
            .ok()
            .flatten()
            .map(|h| h.short().to_string())
            .unwrap_or_else(|| "—".to_string());
        let mut line = format!("{}{}  [{}]  {}", marker, br.name, status, head_short);
        if let Some(ref desc) = br.description {
            line.push_str(&format!("  — {desc}"));
        }
        items.push(line);
    }

    let idx = Select::new()
        .with_prompt("Switch to which branch?")
        .items(&items)
        .default(default_idx)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    Ok(branches[idx].name.clone())
}

/// Update the working directory to match a manifest.
pub fn update_working_dir(
    root: &Path,
    repo: &dyn Repository,
    manifest: &oak_core::Manifest,
) -> Result<()> {
    let ignore = IgnorePatterns::new(root)?;

    // Active scope (if any) restricts which entries are written and which
    // out-of-tree files are considered "untracked". Without this filter,
    // switch would delete every out-of-scope file the user just chose to
    // exclude via --team / --project.
    let prefixes = super::project::active_prefixes(repo)?;

    // Restore files from manifest, collecting a fresh stat-cache row for each
    // file we write so the cache reflects this branch's content rather than a
    // stale row from the branch we switched away from — without this, the next
    // scan can trust that stale row and record a foreign blob.
    let mut cache_upserts = Vec::new();
    for entry in &manifest.entries {
        if !prefixes.is_empty() && !oak_core::path_in_any_prefix(&prefixes, &entry.path) {
            continue;
        }
        let file_path = root.join(&entry.path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(blob) = repo.get_blob(&entry.blob_hash)? {
            fs::write(&file_path, &blob.content)?;
            crate::file_permissions::apply_file_permissions(&file_path, entry.mode)?;
            if entry.mode != oak_core::FileMode::Symlink {
                if let Some(u) = crate::commands::commit::stat_cache_upsert(
                    &entry.path,
                    &file_path,
                    &entry.blob_hash,
                ) {
                    cache_upserts.push(u);
                }
            }
        }
    }

    // Files counted as "tracked" for the delete pass are the in-scope subset;
    // out-of-scope manifest entries don't have on-disk representations under
    // a project scope, so excluding them here is correct.
    let head_files: HashSet<&str> = manifest
        .entries
        .iter()
        .filter(|e| prefixes.is_empty() || oak_core::path_in_any_prefix(&prefixes, &e.path))
        .map(|e| e.path.as_str())
        .collect();
    delete_untracked(root, root, &ignore, &head_files, &prefixes)?;
    clean_empty_dirs(root, &ignore)?;

    // Mirror the materialized tree into the stat cache (scope-aware: only
    // in-scope rows are pruned, since out-of-scope files were left untouched).
    crate::commands::commit::refresh_stat_cache_after_materialize(repo, cache_upserts, &prefixes)?;

    Ok(())
}

/// Delete files not in the manifest. When a project scope is active, files
/// outside the scope are left alone — the switch operation is filtering,
/// not pruning.
fn delete_untracked(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
    head_files: &HashSet<&str>,
    prefixes: &[String],
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
        // Out-of-scope files are not part of the scoped working tree; we
        // don't own the right to delete them.
        if !prefixes.is_empty() {
            let rel_str = relative.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if !oak_core::dir_in_any_prefix(prefixes, &rel_str) {
                    continue;
                }
            } else if !oak_core::path_in_any_prefix(prefixes, &rel_str) {
                continue;
            }
        }
        if path.is_dir() {
            delete_untracked(&path, root, ignore, head_files, prefixes)?;
        } else if path.is_file() {
            let rel_str = relative.to_string_lossy();
            if !head_files.contains(rel_str.as_ref()) {
                fs::remove_file(&path)?;
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
