use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use oak_core::{BranchStatus, OakError, Repository, Result};

use crate::output;

/// Show the status of the working directory
pub fn run(path: &Path) -> Result<()> {
    // When there's no repository here, don't bubble up the raw `RepoNotFound`
    // (or a confusing IO error from scanning the wrong tree) — explain what's
    // going on and offer to create a repo right here.
    let ctx = match crate::resolve::resolve(path) {
        Ok(ctx) => ctx,
        Err(OakError::RepoNotFound) => return not_a_repo(path),
        Err(e) => return Err(e),
    };
    let repo = ctx.open()?;

    if let Some(plan) = crate::commands::merge::plan_remote_merge_reconcile(
        repo.as_ref(),
        crate::commands::merge::RemoteMergeReconcileScope::CurrentBranch,
    )? {
        match crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir) {
            Ok(lock) => {
                let (changes, _, _) =
                    crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;
                if let Some(reconciled) = crate::commands::merge::apply_remote_merge_reconcile(
                    &lock,
                    repo.as_ref(),
                    &ctx.work_tree,
                    changes.is_empty(),
                    plan,
                )? {
                    crate::commands::merge::print_remote_merge_reconcile(&reconciled);
                    output::blank();
                }
            }
            Err(OakError::RepoLocked) => {
                // `oak status` should remain inspectable while another Oak
                // process owns the workdir. Skip the repair for this run and
                // print the current state below.
            }
            Err(e) => return Err(e),
        }
    }

    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;

    print_status(repo.as_ref(), &changes, head, branch_name)?;

    // Last line on purpose: tracked files whose deletions above exist only
    // because ignore rules now cover them are easy to miss inside a big
    // change list, and this is the user's chance to notice before a commit
    // makes it real.
    crate::commands::commit::warn_tracked_now_ignored(
        &changes,
        &ctx.work_tree,
        "will be removed from the branch by the next commit",
    );
    Ok(())
}

/// Show status as one JSON document on stdout.
pub fn run_json(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;

    print_status_json(repo.as_ref(), &ctx, changes, head, branch_name)
}

fn print_status_json(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    changes: Vec<oak_core::FileChange>,
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
) -> Result<()> {
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    let unmerged_commit_count = match branch_name.as_deref() {
        Some(name) => crate::commands::commit::unmerged_commit_count(repo, name)?,
        None => 0,
    };
    let changes = changes
        .into_iter()
        .map(|change| output::StatusChangeJson {
            path: change.path,
            status: output::change_type_name(change.change_type).to_string(),
            old_path: change.old_path,
        })
        .collect();

    let sync_in_progress =
        ctx.oak_dir.join("SYNC_HEAD").exists() || ctx.work_tree.join(".oak/SYNC_HEAD").exists();

    output::print_json(&output::StatusJson {
        branch: branch_name,
        branch_description: branch.as_ref().and_then(|b| b.description.clone()),
        parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
        head: head.map(|h| h.to_string()),
        branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
        unmerged_commit_count,
        changes,
        merge_in_progress: ctx.oak_dir.join("MERGE_HEAD").exists(),
        sync_in_progress,
    })
}

fn print_status(
    repo: &dyn oak_core::Repository,
    changes: &[oak_core::FileChange],
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        if !changes.is_empty() {
            output::print_line(&output::compact_change_summary(changes));
            output::print_compact_change_sample(changes, 5);
        }
        return Ok(());
    }

    // Committed-but-not-merged work on this branch, surfaced after the Head
    // line below. Computed inside the branch block (where the repo is open and
    // the parent name is known) and printed afterwards so it reads Head →
    // Unmerged.
    let mut unmerged_note: Option<String> = None;

    // Show current branch
    if let Some(ref name) = branch_name {
        output::print_line(&format!(
            "On branch {}{}{}",
            output::colors::BOLD,
            name,
            output::colors::RESET,
        ));

        if let Ok(Some(branch)) = repo.get_branch(name) {
            let status_color = match branch.status {
                BranchStatus::Open => output::colors::GREEN,
                BranchStatus::Closed => output::colors::DIM,
            };
            output::print_line(&format!(
                "Status: {}{}{}",
                status_color,
                branch.status,
                output::colors::RESET,
            ));
            if let Some(desc) = branch.description.as_deref() {
                // Descriptions are squash-merge messages and can run long;
                // status is re-read constantly (especially by agents), so
                // show the subject line only and note what's been elided.
                let mut lines = desc.lines();
                let first = lines.next().unwrap_or_default();
                let rest = lines.count();
                if rest > 0 {
                    output::print_line(&format!(
                        "Description: {first} (+{rest} more line{})",
                        if rest == 1 { "" } else { "s" }
                    ));
                } else {
                    output::print_line(&format!("Description: {first}"));
                }
            }
            if let Some(parent) = branch.parent_branch.as_deref() {
                output::print_line(&format!("Parent: {parent}"));
            }
            output::print_line(&format!(
                "Created: {}",
                branch.created_at.format("%Y-%m-%d %H:%M:%S UTC")
            ));

            // Commits authored here that haven't been squash-merged
            // into the parent yet. Quiet when there's nothing pending,
            // like the "working directory clean" path below.
            if let Ok(n) = crate::commands::commit::unmerged_commit_count(repo, name) {
                if n > 0 {
                    let target = branch.parent_branch.as_deref().unwrap_or("its parent");
                    unmerged_note = Some(format!(
                        "{n} commit{} not yet merged into {target}",
                        if n == 1 { "" } else { "s" },
                    ));
                }
            }
        }
    }

    // Show current head
    if let Some(ref head_hash) = head {
        output::print_line(&format!(
            "Head: {}{}{}",
            output::colors::CYAN,
            head_hash.short(),
            output::colors::RESET,
        ));
    } else {
        output::print_line(&format!(
            "{}No commits yet{}",
            output::colors::DIM,
            output::colors::RESET,
        ));
    }

    if let Some(note) = unmerged_note {
        output::print_line(&format!(
            "Unmerged: {}{}{}",
            output::colors::YELLOW,
            note,
            output::colors::RESET,
        ));
    }

    output::blank();

    if changes.is_empty() {
        output::success("Nothing to commit, working directory clean");
    } else {
        output::header(&format!("Changes ({})", changes.len()));
        output::blank();
        for change in changes {
            if change.change_type == oak_core::ChangeType::Renamed {
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
    }

    Ok(())
}

/// Handle `oak status` run somewhere that isn't an Oak repository: explain the
/// situation with a quick summary of what's here, then offer to initialize a
/// repo on the spot (interactive only).
fn not_a_repo(path: &Path) -> Result<()> {
    output::warning(&format!(
        "Not an Oak repository (no .oak found in {} or any parent).",
        path.display()
    ));
    output::blank();

    let (files, dirs) = summarize_dir(path);
    output::print_line(&format!(
        "This directory has {} {} and {} {}.",
        files,
        if files == 1 { "file" } else { "files" },
        dirs,
        if dirs == 1 {
            "subdirectory"
        } else {
            "subdirectories"
        },
    ));
    output::blank();

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let create = interactive
        && Confirm::new()
            .with_prompt("Initialize an Oak repository here?")
            .default(false)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    if create {
        output::blank();
        // `create` implies `interactive`, so init runs interactively too.
        crate::commands::init::run(path, true)
    } else {
        output::info("Run 'oak init' to create a repository here, or 'oak clone <repo>' to clone an existing one.");
        Ok(())
    }
}

/// Count the immediate (non-hidden) files and subdirectories in `path`. Stays
/// shallow on purpose — this is just a quick "what's here" hint, and a deep
/// walk could be slow or hit permission-protected dirs. Unreadable dirs yield
/// zeros rather than an error.
fn summarize_dir(path: &Path) -> (usize, usize) {
    let mut files = 0;
    let mut dirs = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => dirs += 1,
                Ok(_) => files += 1,
                Err(_) => {}
            }
        }
    }
    (files, dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::Repository;
    use std::fs;

    #[test]
    fn status_does_not_take_workdir_lock_when_no_remote_merge_reconcile_is_needed() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        let oak_dir = root.join(".oak");
        fs::create_dir_all(&oak_dir).unwrap();

        let repo = oak_core::SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_current_branch("main").unwrap();

        fs::write(oak_dir.join("wdlock"), std::process::id().to_string()).unwrap();

        run(&root).expect("status should stay lock-free unless reconciliation can apply");

        fs::remove_file(oak_dir.join("wdlock")).unwrap();
    }
}
