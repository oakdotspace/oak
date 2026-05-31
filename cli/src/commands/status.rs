use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use oak_core::{BranchStatus, OakError, Result};

use crate::commands::commit::get_status;
use crate::output;

/// Show the status of the working directory
pub fn run(path: &Path) -> Result<()> {
    // When there's no repository here, don't bubble up the raw `RepoNotFound`
    // (or a confusing IO error from scanning the wrong tree) — explain what's
    // going on and offer to create a repo right here.
    if let Err(OakError::RepoNotFound) = crate::resolve::resolve(path) {
        return not_a_repo(path);
    }

    let (changes, head, branch_name) = get_status(path)?;

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

        if let Ok(ctx) = crate::resolve::resolve(path) {
            if let Ok(repo) = ctx.open() {
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
                        output::print_line(&format!("Description: {desc}"));
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
                    if let Ok(n) =
                        crate::commands::commit::unmerged_commit_count(repo.as_ref(), name)
                    {
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
        for change in &changes {
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
