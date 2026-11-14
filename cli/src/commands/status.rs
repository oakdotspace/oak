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
            match crate::workdir_lock::WorkdirLock::try_acquire(&ctx.oak_dir) {
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
                Err(OakError::RepoLocked | OakError::InterruptedWorktreeUpdate { .. }) => {
                    // `oak status` should remain inspectable while another Oak
                    // process owns the workdir, or after one was killed
                    // mid-update. Skip the repair for this run and print the
                    // current state below.
                }
                Err(e) => return Err(e),
            }
        }
    }

    let scan = crate::commands::commit::scan_status(repo.as_ref(), &ctx.work_tree)?;
    let changes = scan.changes;

    print_status(
        repo.as_ref(),
        &changes,
        scan.head,
        scan.branch_name,
        &scan.head_manifest,
    )?;
    if let Some(hint) = crate::commands::commit::kept_spelling_hint(
        &crate::commands::commit::kept_spellings(&ctx.work_tree, &ctx.oak_dir, &changes),
    ) {
        output::blank();
        output::info(&hint);
    }

    if let Some(interrupted) = crate::worktree_update::pending(&ctx.oak_dir) {
        // The change list above mixes the user's edits with whatever the
        // interrupted update had written; say so before anyone commits it.
        output::blank();
        output::warning(&interrupted.message);
    }

    // Last line on purpose: tracked files whose deletions above exist only
    // because ignore rules now cover them are easy to miss inside a big
    // change list, and this is the user's chance to notice before a commit
    // makes it real.
    crate::commands::commit::warn_tracked_now_ignored(
        &changes,
        &ctx.work_tree,
        "will be removed from the branch by the next commit",
    );
    // A sparse checkout hides everything outside its cone; files someone put
    // there would otherwise vanish from view and never be committed.
    crate::commands::commit::warn_ignored_outside_cone(
        &crate::commands::commit::files_outside_cone(repo.as_ref(), &ctx.work_tree)?,
    );
    Ok(())
}

/// Show status as one JSON document on stdout.
pub fn run_json(path: &Path, compact: bool, fingerprint: bool) -> Result<()> {
    // Computed first and appended to the one status document; see
    // `commands::change::fingerprint` (read-only, no remote required).
    let _appendix = if fingerprint {
        let value = super::change::fingerprint(path, &[])?;
        Some(output::JsonAppendix::install("fingerprint", &value)?)
    } else {
        None
    };
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    status_json(repo.as_ref(), &ctx, compact)
}

/// The whole of `oak status --json` against an open repository. Budget: one
/// working-tree scan and one flatten of HEAD's tree ([`scan_status`] does
/// both); everything else in the document is refs, metadata and tree-to-tree
/// diffs that read only the directories the branch changed. The
/// `status_json_loads_the_head_manifest_once` test holds the tree side of
/// that budget, counting both flattens and individual tree reads.
///
/// [`scan_status`]: crate::commands::commit::scan_status
fn status_json(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    compact: bool,
) -> Result<()> {
    let scan = crate::commands::commit::scan_status(repo, &ctx.work_tree)?;
    // Renames that are only the filesystem's spelling of a tracked file are
    // listed as top-level `kept_spellings`, the same field update receipts
    // carry, so they can be told apart from the user's own renames.
    let kept = crate::commands::commit::kept_spellings(&ctx.work_tree, &ctx.oak_dir, &scan.changes);
    output::note_kept_spellings(kept.iter().map(|(a, b)| (a.as_str(), b.as_str())));
    print_status_json(repo, ctx, scan, compact)
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
pub async fn run_agent_state_json(
    path: &Path,
    refresh: bool,
    compact: bool,
    publication_after: Option<&str>,
) -> Result<()> {
    let state =
        crate::work_state::checkout_agent_state_json(path, refresh, publication_after).await?;
    if compact {
        output::print_json(&output::AgentStateCompactJson::from(state))
    } else {
        output::print_json(&state)
    }
}

fn print_status_json(
    repo: &dyn Repository,
    ctx: &crate::resolve::RepoContext,
    scan: crate::commands::commit::StatusScan,
    compact: bool,
) -> Result<()> {
    let crate::commands::commit::StatusScan {
        changes,
        head,
        branch_name,
        head_manifest,
        ..
    } = scan;
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    // Tracked paths whose content the server withheld under path-based
    // permissions (never materialized locally), read off the manifest the
    // scan already loaded.
    let restricted =
        crate::commands::restricted::restricted_paths_in_manifest(repo, &head_manifest);
    drop(head_manifest);
    let unmerged_commit_count = match branch_name.as_deref() {
        Some(name) => crate::commands::commit::unmerged_commit_count(repo, name)?,
        None => 0,
    };
    let changes: Vec<output::StatusChangeJson> =
        changes.into_iter().map(status_change_json).collect();

    let working_changes = output::StatusChangeSetJson {
        base: head.as_ref().map(ToString::to_string),
        // The working tree has no immutable object id until committed.
        head: None,
        changes: changes.clone(),
        caveats: Vec::new(),
    };
    let progress = crate::work_state::checkout_progress_state(ctx);
    let parent_sync_deferred =
        output::ParentSyncDeferredJson::for_branch(repo, branch_name.as_deref());

    if compact {
        let counts = output::status_change_counts(&changes);
        let change_count = changes.len();
        let changes_omitted = change_count.saturating_sub(output::COMPACT_CHANGE_LIMIT);
        let changes_truncated = changes_omitted > 0;
        let mut recommended_next_commands = Vec::new();
        if progress.json.kind.as_deref() == Some("interrupted_update") {
            recommended_next_commands.extend(progress.json.next_commands.iter().cloned());
        }
        if changes_truncated {
            recommended_next_commands.push("oak status --json".to_string());
            recommended_next_commands.push("oak status --porcelain".to_string());
        }
        if let Some(deferred) = &parent_sync_deferred {
            recommended_next_commands.push(deferred.sync_command.clone());
        }
        let stack = branch_name
            .as_deref()
            .and_then(|b| super::switch::stack_for_display(repo, b));
        output::print_json(&output::StatusCompactJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            branch: branch_name,
            parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
            stack,
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
            parent_sync_deferred,
        })?;
        return Ok(());
    }

    // Full status adds committed contribution evidence. Keep this secondary
    // inspection best-effort: a shallow or partially hydrated checkout must
    // still be able to report its ordinary working status, with an explicit
    // caveat instead of turning an additive field into a new hard failure.
    let branch_changes =
        branch_change_set(repo, branch_name.as_deref(), branch.as_ref(), head.as_ref());
    let identity = crate::commands::RepoIdentityJson::from_repo(
        repo,
        Some(&ctx.work_tree),
        branch_name.as_deref(),
    )?;
    let stack = branch_name
        .as_deref()
        .and_then(|b| super::switch::stack_for_display(repo, b));
    output::print_json(&output::StatusJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        branch: branch_name,
        branch_description: branch.as_ref().and_then(|b| b.description.clone()),
        parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
        stack,
        head: head.map(|h| h.to_string()),
        branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
        unmerged_commit_count,
        changes,
        working_changes,
        branch_changes,
        restricted,
        merge_in_progress: progress.merge_in_progress,
        sync_in_progress: progress.sync_in_progress,
        progress_state: progress.json,
        identity,
        parent_sync_deferred,
    })
}

fn branch_change_set(
    repo: &dyn Repository,
    branch_name: Option<&str>,
    branch: Option<&oak_core::Branch>,
    head: Option<&oak_core::Hash>,
) -> Option<output::StatusChangeSetJson> {
    let (branch_name, branch, head) = (branch_name?, branch?, head?);
    let against = branch
        .parent_branch
        .as_deref()
        .unwrap_or(oak_core::DEFAULT_BRANCH);
    let inspected = (|| -> Result<_> {
        // Tree-to-tree: reads only the directories the branch changed, so a
        // status in a large repo doesn't flatten the fork base and the head.
        let (base, base_root, caveats) =
            crate::commands::review::branch_fork_base_root(repo, branch_name, against)?;
        let head_root = crate::commands::review::root_for_head(repo, Some(head))?;
        let committed =
            crate::commands::review::diff_roots_with_content_renames(repo, &base_root, &head_root)?;
        Ok(output::StatusChangeSetJson {
            base: base.map(|hash| hash.to_string()),
            head: Some(head.to_string()),
            changes: committed.into_iter().map(status_change_json).collect(),
            caveats,
        })
    })();
    Some(
        inspected.unwrap_or_else(|error| output::StatusChangeSetJson {
            base: None,
            head: Some(head.to_string()),
            changes: Vec::new(),
            caveats: vec![format!(
                "Committed branch changes are unavailable from this checkout: {error}"
            )],
        }),
    )
}

fn status_change_json(change: oak_core::FileChange) -> output::StatusChangeJson {
    output::StatusChangeJson {
        path: change.path,
        status: output::change_type_name(change.change_type).to_string(),
        old_path: change.old_path,
    }
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
    if progress.merge_in_progress || progress.sync_in_progress || progress.json.in_progress {
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
    let identity = crate::commands::RepoIdentityJson::from_repo(
        repo,
        Some(&ctx.work_tree),
        branch_name.as_deref(),
    )?;

    let stack = branch_name
        .as_deref()
        .and_then(|b| super::switch::stack_for_display(repo, b));
    output::print_json(&output::InfoJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        branch: branch_name,
        branch_description: branch.as_ref().and_then(|b| b.description.clone()),
        parent: branch.as_ref().and_then(|b| b.parent_branch.clone()),
        stack,
        head: head.map(|h| h.to_string()),
        branch_status: branch.as_ref().map(|b| b.status.as_str().to_string()),
        repo_owner: identity.repo_owner,
        repo_name: identity.repo_name,
        remote_url: identity.remote_url,
        merge_in_progress: progress.merge_in_progress,
        sync_in_progress: progress.sync_in_progress,
        progress_state: progress.json,
        repository_root: identity.repository_root,
        web_url: identity.web_url,
        review_url: identity.review_url,
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
    head_manifest: &oak_core::Manifest,
) -> Result<()> {
    let detached_note = || {
        format!(
            "HEAD is detached at {} (no current branch); run `oak switch -c <name>` before committing",
            head.as_ref().map(|hash| hash.as_str()).unwrap_or("(no commit)")
        )
    };
    if !std::io::stdout().is_terminal() {
        // No summary header: the per-file rows already carry everything a
        // piped consumer needs, and status is the hottest agent read path.
        // A detached HEAD is noted on stderr so stdout stays porcelain.
        if branch_name.is_none() {
            eprintln!("oak: {}", detached_note());
        }
        print_status_porcelain(changes);
        return Ok(());
    }

    // Committed-but-not-merged work on this branch, surfaced after the Head
    // line below. Computed inside the branch block (where the repo is open and
    // the parent name is known) and printed afterwards so it reads Head →
    // Unmerged.
    let mut unmerged_note: Option<String> = None;

    if branch_name.is_none() {
        output::print_line(&detached_note());
    }

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
            if let Some(stack) = super::switch::stack_for_display(repo, name) {
                output::print_line(&format!("Stack: {}", stack.join(" <- ")));
            }
            if let Some(deferred) = output::ParentSyncDeferredJson::for_branch(repo, Some(name)) {
                output::print_line(&format!(
                    "{}Parent sync pending:{} '{}' was not synced by `oak pull --branch-only`; run `{}` before merging",
                    output::colors::YELLOW,
                    output::colors::RESET,
                    deferred.parent,
                    deferred.sync_command,
                ));
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
    let restricted = crate::commands::restricted::restricted_paths_in_manifest(repo, head_manifest);
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
    if crate::resolve::global_state_marker_root(path).is_some() {
        // Discovery already explained (on stderr) which global-only entry
        // stopped it and how to recover; offering `oak init` here would be
        // refused for the same reason.
        return Ok(());
    }
    if let Some(root) = crate::resolve::incomplete_oak_root(path) {
        // An incomplete `.oak/` (no oak.db) is not a repository, and status
        // must not turn it into one by creating `oak.db`. Its other contents
        // (e.g. tracked `.oak/workflows`, `.oak/attributes`) may matter, so never suggest deleting it.
        output::warning(&format!(
            "Not an Oak repository: {} looks like an incomplete Oak repository (it has no oak.db).",
            root.join(".oak").display()
        ));
        output::info(&format!(
            "Run `oak init` in {} to create a repository there (existing .oak/ files are kept), or `oak clone <org>/<repo>` into a new directory.",
            root.display()
        ));
        return Ok(());
    }
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

    let interactive = output::can_prompt();
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

    const PAD_DIRS: usize = 16;
    /// Directories in the fixture's HEAD tree: the root, `a`, `b`, `c`,
    /// `scripts`, `pad` and its children — the tree reads one flatten costs.
    const HEAD_TREES: u64 = 6 + PAD_DIRS as u64;

    /// A checkout on `feature` (one commit ahead of `main`, which it has
    /// merged in once) with a dirty working tree and a non-empty restricted
    /// set: every status section that could want HEAD's tree has a reason to.
    fn checkout_ahead_of_main() -> (tempfile::TempDir, oak_core::SqliteRepository) {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();

        let file = |path: &str, content: &str| {
            let bytes = content.as_bytes().to_vec();
            let hash = oak_core::hash_bytes(&bytes);
            repo.store_blob(&oak_core::Blob {
                hash: hash.clone(),
                size: bytes.len() as u64,
                content: bytes,
            })
            .unwrap();
            let on_disk = root.join(path);
            fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
            fs::write(on_disk, content).unwrap();
            oak_core::ManifestEntry {
                path: path.to_string(),
                blob_hash: hash,
                mode: oak_core::FileMode::Regular,
            }
        };
        let commit = |branch: &str, parent: Option<&oak_core::Hash>, merge, entries| {
            let manifest = repo.put_manifest(entries).unwrap();
            repo.put_commit(
                branch.to_string(),
                parent.cloned(),
                merge,
                manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap()
        };

        // Enough untouched directories that flattening HEAD costs far more
        // tree reads than diffing the few directories the branch changed.
        let mut base = vec![file("a/one.txt", "one\n"), file("b/two.txt", "two\n")];
        for i in 0..PAD_DIRS {
            base.push(file(&format!("pad/{i:02}/keep.txt"), &format!("{i}\n")));
        }
        let fork = commit("main", None, None, base.clone());
        let mut main_tree = base.clone();
        main_tree.push(file("b/main-only.txt", "main\n"));
        let main_head = commit("main", Some(&fork), None, main_tree.clone());

        // Withheld by the server: tracked, recorded as restricted, never on disk.
        let secret = oak_core::ManifestEntry {
            path: "scripts/secret.sh".to_string(),
            blob_hash: oak_core::Hash("5e".repeat(32)),
            mode: oak_core::FileMode::Regular,
        };
        repo.set_metadata(MetadataKey::RestrictedBlobs, secret.blob_hash.as_str())
            .unwrap();
        let mut feature_tree = main_tree;
        feature_tree.push(file("c/three.txt", "three\n"));
        feature_tree.push(secret);
        let first = commit("feature", Some(&fork), None, base);
        let feature_head = commit(
            "feature",
            Some(&first),
            Some(main_head.clone()),
            feature_tree,
        );

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_current_branch("feature").unwrap();

        fs::write(root.join("a/one.txt"), "edited\n").unwrap();
        fs::write(root.join("untracked.txt"), "new\n").unwrap();
        (temp, repo)
    }

    /// The tree-load budget of the status-shaped read paths, counted on the
    /// repository handle rather than timed: flattening a snapshot reads every
    /// directory, so on a large repo each extra flatten costs about as much
    /// as the whole status (fb-634).
    ///
    /// Two counters, because a flatten can be spelled two ways. `flattens`
    /// counts `walk_tree`/`get_manifest`. `tree_reads` counts every tree
    /// object fetched, so a flatten driven through a private `get_tree`
    /// closure is caught too: one flatten of HEAD is `HEAD_TREES` reads, and
    /// the budget leaves room for the handful of directories a tree-to-tree
    /// diff touches but not for a second flatten.
    #[test]
    fn status_json_loads_the_head_manifest_once() {
        let (temp, repo) = checkout_ahead_of_main();
        let ctx = crate::resolve::resolve(temp.path()).unwrap();
        let cost = |run: &dyn Fn()| {
            let before = (repo.tree_flatten_count(), repo.tree_read_count());
            run();
            (
                repo.tree_flatten_count() - before.0,
                repo.tree_read_count() - before.1,
            )
        };
        let assert_one_flatten = |what: &str, (flattens, tree_reads): (u64, u64)| {
            assert_eq!(flattens, 1, "{what}: must flatten exactly one tree");
            assert!(
                (HEAD_TREES - 2..2 * (HEAD_TREES - 2)).contains(&tree_reads),
                "{what}: {tree_reads} tree reads is not one flatten of a \
                 ~{HEAD_TREES}-directory tree plus a tree-to-tree diff"
            );
        };

        assert_one_flatten(
            "oak status --json",
            cost(&|| status_json(&repo, &ctx, false).unwrap()),
        );
        assert_one_flatten(
            "oak status --json --compact",
            cost(&|| status_json(&repo, &ctx, true).unwrap()),
        );
        assert_one_flatten(
            "oak status",
            cost(&|| {
                let scan = crate::commands::commit::scan_status(&repo, temp.path()).unwrap();
                print_status(
                    &repo,
                    &scan.changes,
                    scan.head,
                    scan.branch_name,
                    &scan.head_manifest,
                )
                .unwrap();
            }),
        );
        // Fork-point resolution reads commits and probes root trees; it never
        // flattens a snapshot or reads a tree object.
        assert_eq!(
            cost(&|| {
                let (fork, _, _) =
                    crate::commands::review::branch_fork_base_root(&repo, "feature", "main")
                        .unwrap();
                assert!(fork.is_some());
            }),
            (0, 0),
            "resolving the fork point must not read either branch's tree"
        );
        // `oak owners --branch`: the scan's flatten only.
        assert_one_flatten(
            "oak owners --branch",
            cost(&|| {
                let paths =
                    crate::commands::access::branch_changed_paths(&repo, temp.path()).unwrap();
                assert!(paths.contains(&"c/three.txt".to_string()), "{paths:?}");
            }),
        );
        // `oak diff --branch`: the fork base is the only snapshot it diffs.
        assert_one_flatten(
            "oak diff --branch",
            cost(&|| {
                let source =
                    crate::commands::diff::branch_worktree_source(&repo, temp.path()).unwrap();
                assert!(source.fork.is_some());
            }),
        );
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
