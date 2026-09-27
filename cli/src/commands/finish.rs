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

    repo.edit_branch_description(&branch_name, description)
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
            if let OakError::PublicationUnconfirmed {
                reconciliation_commands,
                ..
            } = &e
            {
                let mut commands = reconciliation_commands.clone();
                commands.push("oak status --json".to_string());
                finish_phase_failed_owned(
                    "push",
                    &completed_phases,
                    &["push", "metadata_sync"],
                    format!(
                        "finish could not confirm whether {unpushed} commit(s) on '{branch_name}' were published: {e}"
                    ),
                    None,
                    commands,
                )
            } else {
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
            }
        })?;
        pushed = true;
        completed_phases.push("push".to_string());
        // A push that followed a trusted host move re-targeted the stored
        // remote; publish the metadata to where the commits went.
        if remote_preflight.persist_remote {
            if let Some(moved) = repo
                .get_metadata(MetadataKey::RemoteUrl)?
                .and_then(|remote| crate::commands::push::normalize_remote_url(&remote))
            {
                remote_preflight.remote = moved;
            }
        }
    }

    // `description_synced` must mean a *confirmed* metadata publication of
    // the description finish just staged. A push leg is not that proof by
    // itself: the unmerged-commit count above also counts commits that are
    // already on the server, and a commit-less push returns "Already up to
    // date" before it ever sends the branch row. The push counts only when
    // its receipt acknowledged the staged description (local sync tracking
    // cleared); otherwise publish the branch metadata explicitly and report
    // only that receipt.
    let confirmed_by_push = pushed && description_acknowledged(&ctx, &branch_name);
    let description_synced = if confirmed_by_push {
        true
    } else {
        let synced = sync_description_to_server(
            repo.as_ref(),
            &remote_preflight.remote,
            &remote_preflight.owner,
            &remote_preflight.repo_name,
            &branch_name,
        )
        .await
        .map_err(|e| {
            if let OakError::PublicationUnconfirmed {
                reconciliation_commands,
                ..
            } = &e
            {
                let mut commands = reconciliation_commands.clone();
                commands.push("oak status --json".to_string());
                finish_phase_failed_owned(
                    "metadata_sync",
                    &completed_phases,
                    &["metadata_sync"],
                    format!(
                        "description saved locally, but finish could not confirm whether it synced to the server: {e}"
                    ),
                    None,
                    commands,
                )
            } else {
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
            }
        })?;
        completed_phases.push("metadata_sync".to_string());
        synced
    };

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
    // Credentials are checked (async) in `preflight_finish_remote_status`,
    // before any local mutation or server write: see
    // `credentialless_remote_is_open_loopback`.
    Ok(FinishRemotePreflight {
        remote: remote_url,
        owner,
        repo_name,
        persist_remote: remote.persist,
    })
}

fn finish_auth_missing_error(remote: &str) -> OakError {
    finish_preflight_error(
        "auth_missing",
        format!(
            "oak finish requires credentials for {remote} before it can finalize. Run `oak login -r {remote}` and retry."
        ),
        Some(format!("oak login -r {remote}")),
        &["oak status --json"],
    )
}

/// Whether finalizing against `remote` without any credential is safe to
/// attempt (fb-64 / D2). A hosted server may answer anonymous reads of a
/// public repo with 200 and of a private one with 404, so a successful
/// read-only probe proves nothing about write access; finishing on that basis
/// would edit the description, commit, and even `POST /api/repos` before the
/// first 401. The only credential-free remote accepted is therefore an open
/// loopback `oak serve`:
///
/// 1. the host is a loopback address or `localhost` (Serve refuses to bind a
///    non-loopback address without `--token`), and
/// 2. an anonymous `GET /api/whoami` is not rejected with 401/403. A hosted
///    Oak server and a `--token` Serve both reject anonymous identity
///    requests; an open Serve has no auth layer and does not.
///
/// Any other outcome (non-loopback, 401/403, network error) keeps the
/// pre-mutation `auth_missing` refusal.
pub(crate) async fn credentialless_remote_is_open_loopback(
    client: &reqwest::Client,
    remote: &str,
) -> bool {
    if !remote_host_is_loopback(remote) {
        return false;
    }
    let remote = remote.trim_end_matches('/');
    match client.get(format!("{remote}/api/whoami")).send().await {
        Ok(response) => !matches!(response.status().as_u16(), 401 | 403),
        Err(_) => false,
    }
}

fn remote_host_is_loopback(remote: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(remote) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    if token.is_none()
        && !credentialless_remote_is_open_loopback(&client, &remote_preflight.remote).await
    {
        return Err(finish_auth_missing_error(&remote_preflight.remote));
    }
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
            if token.is_none()
                && !credentialless_remote_is_open_loopback(&client, &remote_preflight.remote).await
            {
                return Err(finish_auth_missing_error(&remote_preflight.remote));
            }
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

/// Whether local synchronization tracking shows the current description of
/// `branch_name` as acknowledged by the server. Backends without tracking
/// (or any read failure) answer `false`, so finish falls back to an explicit,
/// receipt-checked metadata publication.
fn description_acknowledged(ctx: &crate::resolve::RepoContext, branch_name: &str) -> bool {
    ctx.db_path()
        .and_then(|path| oak_core::SqliteRepository::open(&path))
        .and_then(|repo| repo.branch_description_pending(branch_name))
        .is_ok_and(|pending| !pending)
}

fn finish_auth_token(repo: &dyn Repository, remote: &str) -> Option<String> {
    crate::commands::credentials::effective_token_for_repository(remote, repo)
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
    finish_phase_failed_owned(
        phase,
        completed_phases,
        pending_phases,
        message,
        retry_command,
        manual_recovery_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    )
}

fn finish_phase_failed_owned(
    phase: impl Into<String>,
    completed_phases: &[String],
    pending_phases: &[&str],
    message: impl Into<String>,
    retry_command: Option<String>,
    manual_recovery_commands: Vec<String>,
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
        manual_recovery_commands,
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

#[cfg(test)]
mod credentialless_remote_tests {
    use super::remote_host_is_loopback;

    #[test]
    fn only_loopback_hosts_qualify() {
        for remote in [
            "http://127.0.0.1:8193",
            "http://127.9.9.9",
            "http://localhost:1",
            "http://[::1]:9",
        ] {
            assert!(remote_host_is_loopback(remote), "{remote}");
        }
        for remote in [
            "https://oak.space",
            "http://10.0.0.1:8193",
            "http://localhost.example.com",
            "not a url",
        ] {
            assert!(!remote_host_is_loopback(remote), "{remote}");
        }
    }
}
