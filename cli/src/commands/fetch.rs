use std::path::Path;

use oak_core::{OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Refresh the local copy of `main` (and any other server-only parent
/// branches) without touching the working tree or running a merge.
///
/// This is the read-only counterpart to `oak pull`: it downloads the
/// parent branch's HEAD commit, manifest, and any missing blobs into
/// local storage so subsequent offline operations (status, diff, log,
/// merge previews) see the current server state. The user's current
/// branch and working tree are left untouched unless the refreshed main
/// history proves the current branch was already squash-merged on the
/// server. In that case we mirror `oak merge` locally: close the merged
/// branch, switch to a fresh branch parented onto main, and reset the
/// worktree only when it is clean.
///
/// `main` is the only branch fetched today because it's the only branch
/// that lives server-only and gets out of sync with the local DB. Other
/// branches you've already pulled stay in sync via normal `oak pull`.
///
/// The remote follows the same explicit/env/stored/default precedence as push.
/// Stored remotes use the repo metadata path so trusted redirects update the
/// stored remote; one-shot explicit/env/default remotes retry without
/// rewriting an already-linked checkout.
pub async fn run(path: &Path, remote_url: Option<&str>) -> Result<()> {
    let remote = match super::push::resolve_push_remote(path, remote_url)? {
        resolved if resolved.source == super::push::PushRemoteSource::Stored => None,
        resolved => Some(resolved),
    };

    match run_once(path, remote.as_ref().map(|remote| remote.url.as_str())).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            if let Some(remote) = &remote {
                if remote.persist {
                    super::follow_remote_move(path, &remote.url, &origin)?;
                } else {
                    output::info(&format!(
                        "Remote {} has moved to {origin} — retrying for this command",
                        remote.url
                    ));
                }
                run_once(path, Some(&origin)).await
            } else {
                let old = super::stored_remote_url(path)?.unwrap_or_default();
                super::follow_remote_move(path, &old, &origin)?;
                run_once(path, None).await
            }
        }
        result => result,
    }
}

async fn run_once(path: &Path, remote_url: Option<&str>) -> Result<()> {
    let activity = output::activity("Preparing fetch...");
    let ctx = crate::resolve::resolve(path)?;
    let lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    let before = repo.get_branch_head("main").ok().flatten();
    activity.set_message("Fetching main from remote...");
    let after =
        super::sync::fetch_parent_from_server_with_remote(&repo, "main", remote_url).await?;
    activity.finish_and_clear();

    match (before.as_ref(), after.as_ref()) {
        (Some(b), Some(a)) if b == a => output::info("Already up to date"),
        (_, Some(a)) => output::success(&format!("Fetched main → {}", a.short())),
        (_, None) => output::info("Remote has no commits on main yet"),
    }

    let plan = if after.is_some() {
        super::merge::plan_remote_merge_reconcile(
            &repo,
            super::merge::RemoteMergeReconcileScope::CurrentBranch,
        )?
    } else {
        None
    };

    if let Some(plan) = plan {
        let worktree_clean =
            super::commit::worktree_is_clean_without_storing_blobs(&repo, &ctx.work_tree)?;
        if let Some(reconciled) = super::merge::apply_remote_merge_reconcile(
            &lock,
            &repo,
            &ctx.work_tree,
            worktree_clean,
            plan,
        )? {
            super::merge::print_remote_merge_reconcile(&reconciled);
        }
    }

    // With main's history refreshed, sweep local rows for branches whose
    // work already landed on main (merged by another client or in the web
    // UI). Branch-row metadata only — the working tree stays untouched.
    let pruned = super::merge::prune_merged_branches(&repo)?;
    super::merge::print_pruned_branches(&pruned);

    Ok(())
}
