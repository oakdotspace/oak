use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use oak_core::{BranchStatus, MetadataKey, OakError, Repository, Result};

use crate::output;

/// Show the status of the working directory
pub fn run(path: &Path, reconcile: bool) -> Result<()> {
    // When there's no repository here, don't bubble up the raw `RepoNotFound`
    // (or a confusing IO error from scanning the wrong tree) — explain what's
    // going on and offer to create a repo right here.
    let ctx = match crate::resolve::resolve(path) {
        Ok(ctx) => ctx,
        Err(OakError::RepoNotFound) => return not_a_repo(path),
        Err(e) => return Err(e),
    };
    let repo = ctx.open()?;

    if reconcile && has_linked_remote(repo.as_ref())? {
        if let Some(plan) = crate::commands::merge::plan_remote_merge_reconcile(
            repo.as_ref(),
            crate::commands::merge::RemoteMergeReconcileScope::CurrentBranch,
        )? {
            match crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir) {
                Ok(lock) => {
                    let (changes, _, _) = crate::commands::commit::compute_changes_for_status(
                        repo.as_ref(),
                        &ctx.work_tree,
                    )?;
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
    }

    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes_for_status(repo.as_ref(), &ctx.work_tree)?;

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
pub fn run_json(path: &Path, compact: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes_for_status(repo.as_ref(), &ctx.work_tree)?;

    print_status_json(repo.as_ref(), &ctx, changes, head, branch_name, compact)
}

/// Show status as stable compact rows on stdout, independent of TTY/color state.
pub fn run_porcelain(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let (changes, _, _) =
        crate::commands::commit::compute_changes_for_status(repo.as_ref(), &ctx.work_tree)?;

    print_status_porcelain(&changes);
    Ok(())
}

/// Show repository/branch metadata as one compact JSON document on stdout.
pub fn run_info_json(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = match branch_name.as_deref() {
        Some(name) => crate::commands::commit::resolve_effective_head(repo.as_ref(), name)?,
        None => repo.get_head()?,
    };

    print_info_json(repo.as_ref(), &ctx, head, branch_name)
}

/// Show repository/branch metadata as a concise human-readable summary.
pub fn run_info(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = match branch_name.as_deref() {
        Some(name) => crate::commands::commit::resolve_effective_head(repo.as_ref(), name)?,
        None => repo.get_head()?,
    };

    print_info(repo.as_ref(), &ctx, head, branch_name)
}

/// Show the single preflight document agents can use before acting.
pub async fn run_agent_state_json(path: &Path, refresh: bool, compact: bool) -> Result<()> {
    let state = crate::work_state::checkout_agent_state_json(path, refresh).await?;
    if compact {
        output::print_json(&output::AgentStateCompactJson::from(state))
    } else {
        output::print_json(&state)
    }
}

/// Tracked paths in the head manifest whose content the server withheld under
/// path-based permissions (never materialized locally). Tolerant lookups: any
/// gap in head/commit/manifest resolution reads as "none".
fn restricted_paths(repo: &dyn Repository, head: Option<&oak_core::Hash>) -> Vec<String> {
    let Some(head) = head else {
        return Vec::new();
    };
    let Ok(Some(commit)) = repo.get_commit(head) else {
        return Vec::new();
    };
    let Ok(Some(manifest)) = repo.get_manifest(&commit.manifest_hash) else {
        return Vec::new();
    };
    crate::commands::restricted::restricted_paths_in_manifest(repo, &manifest)
}

fn print_status_json(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    changes: Vec<oak_core::FileChange>,
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
    compact: bool,
) -> Result<()> {
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    let restricted = restricted_paths(repo, head.as_ref());
    let unmerged_commit_count = match branch_name.as_deref() {
        Some(name) => crate::commands::commit::unmerged_commit_count(repo, name)?,
        None => 0,
    };
    let changes: Vec<output::StatusChangeJson> = changes
        .into_iter()
        .map(|change| output::StatusChangeJson {
            path: change.path,
            status: output::change_type_name(change.change_type).to_string(),
            old_path: change.old_path,
        })
        .collect();

    let progress = crate::work_state::checkout_progress_state(ctx);

    if compact {
        let counts = output::status_change_counts(&changes);
        let change_count = changes.len();
        let changes_omitted = change_count.saturating_sub(output::COMPACT_CHANGE_LIMIT);
        let changes_truncated = changes_omitted > 0;
        let mut recommended_next_commands = Vec::new();
        if changes_truncated {
            recommended_next_commands.push("oak status --json".to_string());
            recommended_next_commands.push("oak status --porcelain".to_string());
        }
        output::print_json(&output::StatusCompactJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            branch: branch_name,
            parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
            head: head.map(|h| h.to_string()),
            branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
            dirty: change_count > 0,
            change_count,
            change_counts: counts,
            changes: changes
                .into_iter()
                .take(output::COMPACT_CHANGE_LIMIT)
                .collect(),
            changes_omitted,
            changes_truncated,
            restricted_count: restricted.len(),
            merge_in_progress: progress.merge_in_progress,
            sync_in_progress: progress.sync_in_progress,
            progress_state: progress.json,
            recommended_next_commands,
            mount: None,
        })?;
        return Ok(());
    }

    output::print_json(&output::StatusJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        branch: branch_name,
        branch_description: branch.as_ref().and_then(|b| b.description.clone()),
        parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
        head: head.map(|h| h.to_string()),
        branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
        unmerged_commit_count,
        changes,
        restricted,
        merge_in_progress: progress.merge_in_progress,
        sync_in_progress: progress.sync_in_progress,
        progress_state: progress.json,
    })
}

fn has_linked_remote(repo: &dyn Repository) -> Result<bool> {
    Ok(repo.get_metadata(MetadataKey::RemoteUrl)?.is_some()
        && repo.get_metadata(MetadataKey::RepoOwner)?.is_some()
        && repo.get_metadata(MetadataKey::RepoName)?.is_some())
}

fn short_hash(hash: Option<&oak_core::Hash>) -> String {
    hash.map(|h| {
        let full = h.to_string();
        full[..12.min(full.len())].to_string()
    })
    .unwrap_or_else(|| "(none)".to_string())
}

fn branch_description_summary(description: Option<&String>) -> String {
    let Some(description) = description else {
        return "(none)".to_string();
    };
    let first_line = description
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if first_line.is_empty() {
        return "(empty)".to_string();
    }
    const MAX_DESCRIPTION_CHARS: usize = 120;
    let mut chars = first_line.chars();
    let summary: String = chars.by_ref().take(MAX_DESCRIPTION_CHARS).collect();
    if chars.next().is_some() {
        format!("{summary}...")
    } else {
        summary
    }
}

fn print_info(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
) -> Result<()> {
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    let progress = crate::work_state::checkout_progress_state(ctx);
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?;
    let name = repo.get_metadata(MetadataKey::RepoName)?;
    let repo_label = match (owner.as_deref(), name.as_deref()) {
        (Some(owner), Some(name)) => format!("{owner}/{name}"),
        _ => "(unlinked)".to_string(),
    };

    output::print_line(&format!("Repository: {repo_label}"));
    output::print_line(&format!(
        "Remote: {}",
        repo.get_metadata(MetadataKey::RemoteUrl)?
            .unwrap_or_else(|| "(none)".to_string())
    ));
    output::print_line(&format!(
        "Branch: {}",
        branch_name.unwrap_or_else(|| "(detached)".to_string())
    ));
    output::print_line(&format!(
        "Parent: {}",
        branch
            .as_ref()
            .and_then(|b| b.parent_branch.clone())
            .unwrap_or_else(|| "(none)".to_string())
    ));
    output::print_line(&format!("Head: {}", short_hash(head.as_ref())));
    output::print_line(&format!(
        "Status: {}",
        branch
            .as_ref()
            .map(|b| b.status.as_str().to_string())
            .unwrap_or_else(|| "(unknown)".to_string())
    ));
    output::print_line(&format!(
        "Description: {}",
        branch_description_summary(branch.as_ref().and_then(|b| b.description.as_ref()))
    ));
    if progress.merge_in_progress || progress.sync_in_progress {
        output::print_line(&format!(
            "Progress: {}",
            progress
                .json
                .kind
                .unwrap_or_else(|| "operation in progress".to_string())
        ));
    } else {
        output::print_line("Progress: none");
    }
    Ok(())
}

fn print_info_json(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
) -> Result<()> {
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    let progress = crate::work_state::checkout_progress_state(ctx);

    output::print_json(&output::InfoJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        branch: branch_name,
        branch_description: branch.as_ref().and_then(|b| b.description.clone()),
        parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
        head: head.map(|h| h.to_string()),
        branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
        repo_owner: repo.get_metadata(MetadataKey::RepoOwner)?,
        repo_name: repo.get_metadata(MetadataKey::RepoName)?,
        remote_url: repo.get_metadata(MetadataKey::RemoteUrl)?,
        merge_in_progress: progress.merge_in_progress,
        sync_in_progress: progress.sync_in_progress,
        progress_state: progress.json,
    })
}

fn print_status_porcelain(changes: &[oak_core::FileChange]) {
    for change in changes {
        output::print_line(&output::compact_change_line(change));
    }
}

fn print_status(
    repo: &dyn oak_core::Repository,
    changes: &[oak_core::FileChange],
    head: Option<oak_core::Hash>,
    branch_name: Option<String>,
) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        // No summary header: the per-file rows already carry everything a
        // piped consumer needs, and status is the hottest agent read path.
        print_status_porcelain(changes);
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
            // like the "Nothing to commit" path below.
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
        output::success("Nothing to commit");
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

    // Files the server withheld under path-based permissions: present in the
    // branch but never materialized here. Not deletions — kept out of the
    // change list above so a commit can't sweep them off the branch.
    let restricted = restricted_paths(repo, head.as_ref());
    if !restricted.is_empty() {
        output::blank();
        output::header(&format!("Restricted ({})", restricted.len()));
        output::blank();
        for path in &restricted {
            output::item(&format!(
                "{}{}{} (content withheld — {})",
                output::colors::DIM,
                path,
                output::colors::RESET,
                crate::commands::restricted::ACCESS_HINT,
            ));
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

        run(&root, false).expect("status should stay lock-free unless reconciliation can apply");

        fs::remove_file(oak_dir.join("wdlock")).unwrap();
    }

    #[test]
    fn linked_remote_requires_url_owner_and_repo_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        assert!(!has_linked_remote(&repo).unwrap());

        repo.set_metadata(oak_core::MetadataKey::RemoteUrl, "https://oak.example")
            .unwrap();
        assert!(!has_linked_remote(&repo).unwrap());

        repo.set_metadata(oak_core::MetadataKey::RepoOwner, "oak")
            .unwrap();
        assert!(!has_linked_remote(&repo).unwrap());

        repo.set_metadata(oak_core::MetadataKey::RepoName, "demo")
            .unwrap();
        assert!(has_linked_remote(&repo).unwrap());
    }
}
