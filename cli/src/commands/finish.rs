use std::path::Path;

use oak_core::{BranchStatus, MetadataKey, OakError, Repository, Result};
use serde::Serialize;

use crate::output;

#[derive(Debug, Serialize)]
pub struct FinishJson {
    pub schema_version: u32,
    pub context: String,
    pub branch: String,
    pub branch_description: String,
    pub phase: String,
    pub completed_phases: Vec<String>,
    pub pending_phases: Vec<String>,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
    pub branch_url: Option<String>,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub committed: bool,
    pub pushed: bool,
    pub description_synced: bool,
    pub unpushed_before: usize,
    pub unpushed_after: usize,
}

pub async fn run(path: &Path, description: &str) -> Result<()> {
    run_inner(path, description).await.map(|_| ())
}

pub async fn run_json(path: &Path, description: &str) -> Result<FinishJson> {
    output::begin_capture();
    let result = run_inner(path, description).await;
    let _captured = output::end_capture();
    result
}

struct FinishRemotePreflight {
    remote: String,
    owner: String,
    repo_name: String,
    persist_remote: bool,
}

async fn run_inner(path: &Path, description: &str) -> Result<FinishJson> {
    if description.trim().is_empty() {
        return Err(finish_preflight_error(
            "invalid_description",
            "oak finish requires a non-empty --desc or --desc-file",
            Some("oak finish --desc-file <file> --json".to_string()),
            &["oak status --json"],
        ));
    }

    let ctx = crate::resolve::resolve(path)?;
    if ctx.oak_dir.join("MERGE_HEAD").exists() {
        return Err(finish_preflight_error(
            "merge_in_progress",
            "A merge is already in progress. Resolve or abort it before `oak finish`.",
            None,
            &["oak merge --continue", "oak merge --abort"],
        ));
    }
    if ctx.oak_dir.join("SYNC_HEAD").exists() || ctx.work_tree.join(".oak/SYNC_HEAD").exists() {
        return Err(finish_preflight_error(
            "sync_in_progress",
            "sync is in progress; run `oak pull --continue` after resolving conflicts, \
             or `oak pull --abort`, before `oak finish`",
            None,
            &["oak pull --continue", "oak pull --abort"],
        ));
    }

    let repo = ctx.open()?;
    let branch_name = repo
        .get_current_branch_name()?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            finish_preflight_error(
                "no_current_branch",
                "oak finish requires a current branch",
                Some("oak status --json".to_string()),
                &[],
            )
        })?;
    let branch = repo.get_branch(&branch_name)?.ok_or_else(|| {
        finish_preflight_error(
            "branch_not_found",
            format!("Branch not found: {branch_name}"),
            Some("oak status --json".to_string()),
            &[],
        )
    })?;
    if branch.status == BranchStatus::Closed {
        return Err(finish_preflight_error(
            "branch_closed",
            format!("Branch '{branch_name}' is closed"),
            Some("oak status --json".to_string()),
            &[],
        ));
    }
    let mut remote_preflight = require_finish_remote_preconditions(&ctx.work_tree, repo.as_ref())?;
    let head_before = crate::commands::commit::resolve_effective_head(repo.as_ref(), &branch_name)?
        .map(|h| h.to_string());
    let (changes, _, _) = crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;
    let was_dirty = !changes.is_empty();
    let unpushed_before =
        crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?;
    preflight_finish_remote_status(
        &ctx.work_tree,
        repo.as_ref(),
        &mut remote_preflight,
        was_dirty || unpushed_before > 0,
    )
    .await?;

    let mut completed_phases = vec!["preflight".to_string()];

    repo.update_branch_description(&branch_name, description)
        .map_err(|e| {
            finish_phase_failed(
                "description",
                &completed_phases,
                &["description", "commit", "push", "metadata_sync"],
                format!("finish could not stage branch description: {e}"),
                Some("oak finish --desc-file <file> --json".to_string()),
                &["oak desc --file <file>"],
            )
        })?;
    completed_phases.push("description".to_string());

    let mut committed = false;
    if was_dirty {
        crate::commands::commit::run(&ctx.work_tree).map_err(|e| {
            finish_phase_failed(
                "commit",
                &completed_phases,
                &["commit", "push", "metadata_sync"],
                format!(
                    "finish could not commit dirty work: {e}. Run `oak commit`, then `oak push`."
                ),
                Some("oak commit".to_string()),
                &["oak status --json"],
            )
        })?;
        committed = true;
        completed_phases.push("commit".to_string());

        let (remaining, _, _) =
            crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;
        if !remaining.is_empty() {
            return Err(finish_phase_failed(
                "commit",
                &completed_phases,
                &["push", "metadata_sync"],
                format!(
                    "finish could not save all dirty work ({} change(s) remain). \
                     Run `oak status`, then `oak commit`.",
                    remaining.len()
                ),
                Some("oak status --json".to_string()),
                &["oak commit"],
            ));
        }
    }

    let unpushed = crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?;
    let mut pushed = false;
    let description_synced;
    if unpushed > 0 {
        crate::commands::push::run_resolved(
            &ctx.work_tree,
            &remote_preflight.remote,
            remote_preflight.persist_remote,
            false,
            None,
        )
        .await
        .map_err(|e| {
            finish_phase_failed(
                "push",
                &completed_phases,
                &["push", "metadata_sync"],
                format!(
                    "finish could not push {unpushed} unpushed commit(s) on '{branch_name}': {e}. \
                     Run `oak push` to retry."
                ),
                Some("oak push".to_string()),
                &["oak finish --desc-file <file> --json"],
            )
        })?;
        pushed = true;
        completed_phases.push("push".to_string());
        description_synced = true;
    } else {
        description_synced = sync_description_to_server(
            repo.as_ref(),
            &remote_preflight.remote,
            &remote_preflight.owner,
            &remote_preflight.repo_name,
            &branch_name,
        )
        .await
        .map_err(|e| {
            finish_phase_failed(
                "metadata_sync",
                &completed_phases,
                &["metadata_sync"],
                format!(
                    "description saved locally but could not sync to the server: {e}. \
                     Run `oak desc --file <file>` to retry."
                ),
                Some("oak finish --desc-file <file> --json".to_string()),
                &["oak desc --file <file>"],
            )
        })?;
        completed_phases.push("metadata_sync".to_string());
    }

    let unpushed_after = if pushed {
        0
    } else {
        crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?
    };
    if unpushed_after > 0 {
        return Err(finish_phase_failed(
            "push",
            &completed_phases,
            &["push", "metadata_sync"],
            format!(
                "finish pushed but still sees {unpushed_after} unpushed commit(s) on '{branch_name}'. \
                 Run `oak push` to retry."
            ),
            Some("oak push".to_string()),
            &["oak status --json"],
        ));
    }

    let head_after = crate::commands::commit::resolve_effective_head(repo.as_ref(), &branch_name)?
        .map(|h| h.to_string());
    let branch_url = branch_web_url(repo.as_ref(), &branch_name)?;
    output::success(&format!("Finished branch '{branch_name}'"));
    Ok(FinishJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        context: "checkout".to_string(),
        branch: branch_name,
        branch_description: description.to_string(),
        phase: "complete".to_string(),
        completed_phases,
        pending_phases: Vec::new(),
        retry_command: None,
        manual_recovery_commands: Vec::new(),
        branch_url,
        head_before,
        head_after,
        committed,
        pushed,
        description_synced,
        unpushed_before,
        unpushed_after,
    })
}

fn require_finish_remote_preconditions(
    path: &Path,
    repo: &dyn Repository,
) -> Result<FinishRemotePreflight> {
    let remote = crate::commands::push::resolve_push_remote(path, None)?;
    if remote.source == crate::commands::push::PushRemoteSource::Default {
        return Err(finish_preflight_error(
            "remote_not_configured",
            "oak finish requires a linked remote before it can finalize. Run `oak push --repo <org>/<repo>` once to link this checkout, or run `oak push` if it is already configured.",
            Some(crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string()),
            &["oak info --json"],
        ));
    }
    let remote_url = remote.url.trim_end_matches('/').to_string();
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        finish_preflight_error(
            "remote_identity_missing",
            "oak finish requires a linked remote owner before it can finalize. Run `oak push --repo <org>/<repo>` once to link this checkout.",
            Some(crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string()),
            &["oak info --json"],
        )
    })?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        finish_preflight_error(
            "remote_identity_missing",
            "oak finish requires a linked remote repository name before it can finalize. Run `oak push --repo <org>/<repo>` once to link this checkout.",
            Some(crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string()),
            &["oak info --json"],
        )
    })?;
    if finish_auth_token(repo, &remote_url).is_none() {
        return Err(finish_preflight_error(
            "auth_missing",
            format!(
                "oak finish requires credentials for {remote_url} before it can finalize. Run `oak login -r {remote_url}` and retry."
            ),
            Some(format!("oak login -r {remote_url}")),
            &["oak status --json"],
        ));
    }
    Ok(FinishRemotePreflight {
        remote: remote_url,
        owner,
        repo_name,
        persist_remote: remote.persist,
    })
}

fn finish_missing_remote_repo_error(remote: &str, endpoint: &str) -> OakError {
    finish_preflight_error(
        "remote_repo_missing",
        format!(
            "oak finish reached {endpoint} at {remote}, but the remote repo does not exist and this finish has no push phase to create it."
        ),
        Some("oak push".to_string()),
        &["oak status --json"],
    )
}

async fn preflight_finish_remote_status(
    path: &Path,
    repo: &dyn Repository,
    remote_preflight: &mut FinishRemotePreflight,
    push_phase_will_create_missing_repo: bool,
) -> Result<()> {
    let client = crate::http::api_client();
    let endpoint = format!("{}/{}", remote_preflight.owner, remote_preflight.repo_name);
    let token = finish_auth_token(repo, &remote_preflight.remote);
    match crate::commands::push::preflight_remote_repo(
        &client,
        &remote_preflight.remote,
        &endpoint,
        token.as_deref(),
    )
    .await
    {
        Ok(crate::commands::push::RemoteRepoPreflight::Exists) => Ok(()),
        Ok(crate::commands::push::RemoteRepoPreflight::MissingWillCreate)
            if push_phase_will_create_missing_repo =>
        {
            Ok(())
        }
        Ok(crate::commands::push::RemoteRepoPreflight::MissingWillCreate) => Err(
            finish_missing_remote_repo_error(&remote_preflight.remote, &endpoint),
        ),
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            let old_remote = remote_preflight.remote.clone();
            if remote_preflight.persist_remote {
                crate::commands::follow_remote_move(path, &old_remote, &origin)?;
            } else {
                output::info(&format!(
                    "Remote {old_remote} has moved to {origin} — retrying for this command"
                ));
            }
            remote_preflight.remote = origin.trim_end_matches('/').to_string();
            let token = finish_auth_token(repo, &remote_preflight.remote);
            match crate::commands::push::preflight_remote_repo(
                &client,
                &remote_preflight.remote,
                &endpoint,
                token.as_deref(),
            )
            .await
            {
                Ok(crate::commands::push::RemoteRepoPreflight::Exists) => Ok(()),
                Ok(crate::commands::push::RemoteRepoPreflight::MissingWillCreate)
                    if push_phase_will_create_missing_repo =>
                {
                    Ok(())
                }
                Ok(crate::commands::push::RemoteRepoPreflight::MissingWillCreate) => Err(
                    finish_missing_remote_repo_error(&remote_preflight.remote, &endpoint),
                ),
                Err(e) => Err(finish_remote_preflight_error(
                    &remote_preflight.remote,
                    &endpoint,
                    e,
                )),
            }
        }
        Err(e) => Err(finish_remote_preflight_error(
            &remote_preflight.remote,
            &endpoint,
            e,
        )),
    }
}

fn finish_remote_preflight_error(remote: &str, endpoint: &str, error: OakError) -> OakError {
    if is_remote_auth_error(&error) {
        finish_preflight_error(
            "auth_failed",
            format!(
                "oak finish could not authenticate to remote repo {endpoint} at {remote} before mutating local state: {error}. Run `oak login -r {remote}` and retry."
            ),
            Some(format!("oak login -r {remote}")),
            &["oak status --json"],
        )
    } else {
        finish_preflight_error(
            "remote_unreachable",
            format!(
                "oak finish could not reach remote repo {endpoint} at {remote} before mutating local state: {error}",
            ),
            Some("oak finish --desc-file <file> --json".to_string()),
            &["oak status --json", "oak push"],
        )
    }
}

fn is_remote_auth_error(error: &OakError) -> bool {
    matches!(error, OakError::Server(message) if message.contains("HTTP 401") || message.contains("HTTP 403"))
}

async fn sync_description_to_server(
    repo: &dyn Repository,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
) -> Result<bool> {
    let remote = remote.trim_end_matches('/').to_string();
    let token = finish_auth_token(repo, &remote);
    crate::commands::push::push_branch_metadata(
        repo,
        &remote,
        owner,
        repo_name,
        branch_name,
        token.as_deref(),
    )
    .await?;
    Ok(true)
}

fn finish_auth_token(repo: &dyn Repository, remote: &str) -> Option<String> {
    std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| crate::commands::credentials::get_token_for_server(remote))
}

fn finish_preflight_error(
    blocker: impl Into<String>,
    message: impl Into<String>,
    retry_command: Option<String>,
    manual_recovery_commands: &[&str],
) -> OakError {
    OakError::FinishPreflight(Box::new(oak_core::FinishPreflightError {
        blocker: blocker.into(),
        message: message.into(),
        pending_phases: vec![
            "description".to_string(),
            "commit".to_string(),
            "push".to_string(),
            "metadata_sync".to_string(),
        ],
        retry_command,
        manual_recovery_commands: manual_recovery_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    }))
}

fn finish_phase_failed(
    phase: impl Into<String>,
    completed_phases: &[String],
    pending_phases: &[&str],
    message: impl Into<String>,
    retry_command: Option<String>,
    manual_recovery_commands: &[&str],
) -> OakError {
    OakError::FinishPhaseFailed(Box::new(oak_core::FinishPhaseError {
        phase: phase.into(),
        completed_phases: completed_phases.to_vec(),
        pending_phases: pending_phases
            .iter()
            .map(|phase| (*phase).to_string())
            .collect(),
        message: message.into(),
        retry_command,
        manual_recovery_commands: manual_recovery_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    }))
}

fn branch_web_url(repo: &dyn Repository, branch_name: &str) -> Result<Option<String>> {
    let Some(remote) = repo.get_metadata(MetadataKey::RemoteUrl)? else {
        return Ok(None);
    };
    let Some(owner) = repo.get_metadata(MetadataKey::RepoOwner)? else {
        return Ok(None);
    };
    let Some(repo_name) = repo.get_metadata(MetadataKey::RepoName)? else {
        return Ok(None);
    };
    Ok(Some(crate::commands::branch_web_url(
        &remote,
        &format!("{owner}/{repo_name}"),
        branch_name,
    )))
}
