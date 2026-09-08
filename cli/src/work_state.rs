//! Shared agent-facing work state for regular checkouts and mounts.

use std::path::Path;

use oak_core::{BranchStatus, MetadataKey, Result, SqliteRepository};
use serde::Deserialize;
use serde::Serialize;

use crate::agent_action::{self, AgentActionContext, AgentActionState, RemoteFreshnessInput};
use crate::commands;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
/// Full `oak agent state --json` v2 removes the redundant top-level
/// `can_finish` alias. Consumers should use `finish_eligible`.
pub const AGENT_STATE_SCHEMA_VERSION: u32 = 2;
/// Compact agent-state JSON remains v1 because it never emitted `can_finish`.
pub const AGENT_STATE_COMPACT_SCHEMA_VERSION: u32 = 1;
const CHECKOUT_PUSH_RECEIPT_FILE: &str = "LAST_PUSHED_HEAD.json";

#[derive(Serialize, Deserialize)]
struct CheckoutPushReceipt {
    schema_version: u32,
    remote: String,
    owner: String,
    repo: String,
    branch: String,
    head: Option<String>,
    source: String,
}

struct CheckoutPushObservation {
    head: Option<String>,
    source: String,
}

/// Remember the exact branch/head accepted by a successful push. This is a
/// local receipt, not a claim that the remote can never move afterward;
/// `agent state --refresh` remains the authoritative remote check.
pub(crate) fn record_checkout_push_success(
    oak_dir: &Path,
    remote: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    head: &str,
) -> Result<()> {
    record_checkout_remote_observation(
        oak_dir,
        remote,
        owner,
        repo,
        branch,
        Some(head),
        "local_push_receipt",
    )
}

fn record_checkout_remote_observation(
    oak_dir: &Path,
    remote: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    head: Option<&str>,
    source: &str,
) -> Result<()> {
    let remote = commands::push::normalize_remote_url(remote).ok_or_else(|| {
        oak_core::OakError::InvalidArgument(
            "cannot persist pushed-head receipt for an invalid remote URL".to_string(),
        )
    })?;
    let bytes = serde_json::to_vec(&CheckoutPushReceipt {
        schema_version: 2,
        remote,
        owner: owner.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        head: head.map(ToString::to_string),
        source: source.to_string(),
    })
    .map_err(|error| oak_core::OakError::Database(error.to_string()))?;
    crate::atomic_file::write_atomic(&oak_dir.join(CHECKOUT_PUSH_RECEIPT_FILE), bytes)
}

fn load_checkout_push_receipt(
    oak_dir: &Path,
    remote: &str,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Option<CheckoutPushObservation> {
    let bytes = std::fs::read(oak_dir.join(CHECKOUT_PUSH_RECEIPT_FILE)).ok()?;
    let receipt: CheckoutPushReceipt = serde_json::from_slice(&bytes).ok()?;
    let source = match receipt.source.as_str() {
        "local_push_receipt" | "remote_refresh_cache" => receipt.source,
        _ => return None,
    };
    (receipt.schema_version == 2
        && receipt.remote == remote
        && receipt.owner == owner
        && receipt.repo == repo
        && receipt.branch == branch)
        .then_some(CheckoutPushObservation {
            head: receipt.head,
            source,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkContext {
    Checkout,
    Mount,
}

impl WorkContext {
    fn as_str(self) -> &'static str {
        match self {
            WorkContext::Checkout => "checkout",
            WorkContext::Mount => "mount",
        }
    }
}

#[derive(Debug)]
pub struct AgentWorkState {
    pub context: WorkContext,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub remote_url: Option<String>,
    pub branch: Option<String>,
    pub branch_description: Option<String>,
    pub branch_description_present: bool,
    pub head: Option<String>,
    pub local_parent_head: Option<String>,
    pub remote_parent_head: Option<String>,
    pub remote_parent_fetched_at: Option<String>,
    pub current_branch_pushed_head: Option<String>,
    pub current_branch_push_checked: bool,
    pub current_branch_push_source: Option<String>,
    pub refresh_requested: bool,
    pub refresh_supported: bool,
    pub refresh_errors: Vec<String>,
    pub needs_pull: bool,
    pub needs_push: bool,
    pub can_finish: bool,
    pub dirty: bool,
    pub changes: Vec<output::StatusChangeJson>,
    pub unpushed_commit_count: usize,
    pub progress_state: output::ProgressStateJson,
    pub recommended_next_commands: Vec<String>,
    pub recommended_action: output::AgentRecommendedActionJson,
    pub blocking_reason: Option<String>,
    pub finish_eligible: bool,
    pub mount: Option<serde_json::Value>,
}

impl From<AgentWorkState> for output::AgentStateJson {
    fn from(state: AgentWorkState) -> Self {
        output::AgentStateJson {
            schema_version: AGENT_STATE_SCHEMA_VERSION,
            context: state.context.as_str().to_string(),
            repo_owner: state.repo_owner,
            repo_name: state.repo_name,
            remote_url: state.remote_url,
            branch: state.branch,
            branch_description: state.branch_description,
            branch_description_present: state.branch_description_present,
            head: state.head,
            local_parent_head: state.local_parent_head,
            remote_parent_head: state.remote_parent_head,
            remote_parent_fetched_at: state.remote_parent_fetched_at,
            current_branch_pushed_head: state.current_branch_pushed_head,
            current_branch_push_checked: state.current_branch_push_checked,
            current_branch_push_source: state.current_branch_push_source,
            refresh_requested: state.refresh_requested,
            refresh_supported: state.refresh_supported,
            refresh_errors: state.refresh_errors,
            needs_pull: state.needs_pull,
            needs_push: state.needs_push,
            dirty: state.dirty,
            changes: state.changes,
            unpushed_commit_count: state.unpushed_commit_count,
            progress_state: state.progress_state,
            recommended_next_commands: state.recommended_next_commands,
            recommended_action: state.recommended_action,
            blocking_reason: state.blocking_reason,
            finish_eligible: state.finish_eligible,
            mount: state.mount,
        }
    }
}

pub async fn checkout_agent_state_json(
    path: &Path,
    refresh: bool,
) -> Result<output::AgentStateJson> {
    Ok(checkout_agent_state(path, refresh).await?.into())
}

pub fn mount_agent_state_json(dest: &Path, refresh: bool) -> Result<output::AgentStateJson> {
    Ok(mount_agent_state(dest, refresh)?.into())
}

pub async fn checkout_agent_state(path: &Path, refresh: bool) -> Result<AgentWorkState> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let (changes, head, branch_name) =
        commands::commit::compute_changes_for_status(repo.as_ref(), &ctx.work_tree)?;
    let branch = match branch_name.as_deref() {
        Some(name) => repo.get_branch(name)?,
        None => None,
    };
    let local_work_commit_count = match branch_name.as_deref() {
        Some(name) => commands::commit::unmerged_commit_count(repo.as_ref(), name)?,
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
    let progress = checkout_progress_state(&ctx);
    let dirty = !changes.is_empty();
    let branch_description = branch.as_ref().and_then(|b| b.description.clone());
    let branch_description_present = description_present(branch_description.as_deref());
    let branch_closed = branch
        .as_ref()
        .is_some_and(|branch| branch.status == BranchStatus::Closed);
    let local_head = head.as_ref().map(ToString::to_string);
    let local_parent_head = branch
        .as_ref()
        .and_then(|b| b.parent_branch.as_deref())
        .map(|parent| {
            repo.get_branch_head(parent)
                .map(|h| h.map(|h| h.to_string()))
        })
        .transpose()?
        .flatten();
    let mut remote_parent_head = None;
    let repo_owner = repo.get_metadata(MetadataKey::RepoOwner)?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?;
    let stored_remote = repo.get_metadata(MetadataKey::RemoteUrl)?;
    let receipt_remote = stored_remote
        .as_deref()
        .and_then(commands::push::normalize_remote_url);
    if refresh {
        if let (Some(stored), Some(normalized)) =
            (stored_remote.as_deref(), receipt_remote.as_deref())
        {
            if stored != normalized {
                repo.set_metadata(MetadataKey::RemoteUrl, normalized)?;
            }
        }
    }
    let receipt = branch_name
        .as_deref()
        .zip(receipt_remote.as_deref())
        .zip(repo_owner.as_deref())
        .zip(repo_name.as_deref())
        .and_then(|(((branch, remote), owner), repo_name)| {
            load_checkout_push_receipt(&ctx.oak_dir, remote, owner, repo_name, branch)
        });
    let mut current_branch_pushed_head = receipt.as_ref().and_then(|receipt| receipt.head.clone());
    let mut current_branch_push_checked = false;
    let mut current_branch_push_source = receipt.map(|receipt| receipt.source);
    let mut refresh_errors = Vec::new();
    if refresh {
        if branch
            .as_ref()
            .and_then(|b| b.parent_branch.as_deref())
            .is_some_and(|parent| parent == oak_core::DEFAULT_BRANCH)
        {
            output::begin_capture();
            let fetched = commands::sync::fetch_parent_from_server_with_remote(
                repo.as_ref(),
                oak_core::DEFAULT_BRANCH,
                receipt_remote.as_deref(),
            )
            .await;
            let _captured = output::end_capture();
            match fetched {
                Ok(head) => remote_parent_head = head.map(|h| h.to_string()),
                Err(err) => refresh_errors.push(format!("remote_parent_head: {err}")),
            }
        }

        if let (Some(remote), Some(owner), Some(repo_name), Some(branch_name)) = (
            receipt_remote.as_deref(),
            repo_owner.as_deref(),
            repo_name.as_deref(),
            branch_name.as_deref(),
        ) {
            let token = commands::credentials::effective_token(
                remote,
                repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
            );
            let fetched = commands::push::fetch_remote_branch_head(
                &crate::http::api_client(),
                remote,
                &format!("{owner}/{repo_name}"),
                branch_name,
                token.as_deref(),
            )
            .await;
            match fetched {
                Ok(head) => {
                    current_branch_push_checked = true;
                    current_branch_pushed_head = head.map(|h| h.to_string());
                    current_branch_push_source = Some("remote_refresh".to_string());
                    if let Err(error) = record_checkout_remote_observation(
                        &ctx.oak_dir,
                        remote,
                        owner,
                        repo_name,
                        branch_name,
                        current_branch_pushed_head.as_deref(),
                        "remote_refresh_cache",
                    ) {
                        // A stale positive receipt must never survive an
                        // authoritative contradiction merely because its
                        // replacement could not be persisted.
                        let _ = std::fs::remove_file(ctx.oak_dir.join(CHECKOUT_PUSH_RECEIPT_FILE));
                        refresh_errors.push(format!("current_branch_pushed_head_cache: {error}"));
                    }
                }
                Err(err) => {
                    current_branch_push_checked = false;
                    refresh_errors.push(format!("current_branch_pushed_head: {err}"));
                }
            }
        }
    }
    let remote_url = receipt_remote;
    let remote_configured = repo_owner.is_some() && repo_name.is_some() && remote_url.is_some();
    let remote_auth_configured = remote_url
        .as_deref()
        .is_some_and(|remote| finish_auth_configured(repo.as_ref(), remote));
    let remote_parent_fetched_at = repo.get_metadata(MetadataKey::MainLastCheckedAt)?;
    let needs_pull = remote_parent_head.is_some() && remote_parent_head != local_parent_head;
    let unpushed_commit_count = refreshed_unpushed_commit_count(
        repo.as_ref(),
        branch_name.as_deref(),
        local_work_commit_count,
        local_head.as_deref(),
        current_branch_pushed_head.as_deref(),
        current_branch_push_checked || current_branch_push_source.is_some(),
    )?;
    let needs_push = if current_branch_push_checked || current_branch_push_source.is_some() {
        match (local_head.as_deref(), current_branch_pushed_head.as_deref()) {
            (Some(local), Some(pushed)) => local != pushed,
            (Some(_), None) => local_work_commit_count > 0,
            _ => false,
        }
    } else {
        local_work_commit_count > 0
    };
    let can_finish = can_finish(
        &progress.json,
        branch_name.as_deref(),
        branch_closed,
        needs_pull,
        remote_configured,
        remote_auth_configured,
    );
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(
        &progress.json,
        branch_name.as_deref(),
        branch_closed,
        needs_pull,
        remote_configured,
        remote_auth_configured,
    );
    let mut recommended_next_commands = checkout_next_commands(
        can_finish,
        branch_closed,
        progress.json.in_progress,
        dirty,
        needs_pull,
        needs_push,
        local_work_commit_count,
        unpushed_commit_count,
        remote_configured,
        remote_auth_configured,
        remote_url.as_deref(),
        &progress.json.next_commands,
    );
    let recommended_action = recommended_action(
        WorkContext::Checkout,
        &recommended_next_commands,
        &progress.json,
        dirty,
        needs_pull,
        needs_push,
        unpushed_commit_count,
        remote_configured,
        refresh,
        true,
        &refresh_errors,
        remote_parent_fetched_at.as_deref(),
        current_branch_push_checked,
        blocking_reason.as_deref(),
    );
    // Make diff evidence discoverable from the agent's front door: whenever
    // there is local work to review, name the exact tier-1 command —
    // summary JSON first, hunks on request. Appended after
    // `recommended_action` derives so the primary action stays what the
    // state machine chose.
    recommended_next_commands.extend(diff_evidence_commands(
        progress.json.in_progress,
        branch_closed,
        dirty,
        local_work_commit_count,
        branch_name.as_deref(),
    ));

    Ok(AgentWorkState {
        context: WorkContext::Checkout,
        repo_owner,
        repo_name,
        remote_url,
        branch: branch_name,
        branch_description,
        branch_description_present,
        head: local_head,
        local_parent_head,
        remote_parent_head,
        remote_parent_fetched_at,
        current_branch_pushed_head,
        current_branch_push_checked,
        current_branch_push_source,
        refresh_requested: refresh,
        refresh_supported: true,
        refresh_errors,
        needs_pull,
        needs_push,
        can_finish,
        dirty,
        changes,
        unpushed_commit_count,
        progress_state: progress.json,
        recommended_next_commands,
        recommended_action,
        blocking_reason,
        finish_eligible,
        mount: None,
    })
}

pub fn mount_agent_state(dest: &Path, refresh: bool) -> Result<AgentWorkState> {
    let (cfg, state_dir) = commands::mount::config_for_dest(dest)?;
    let cache = SqliteRepository::open_relaxed(&commands::mount::state::cache_db_path(&state_dir))?;
    let overlay = commands::mount::state::load_overlay_meta(&state_dir)?;
    let changes = commands::mount::overlay_status_changes(&overlay);
    let progress_state = commands::mount::mount_progress_state(&state_dir)?;
    let (branch, branch_description, _parent, head, branch_status) =
        commands::mount::branch_snapshot(&cache, &cfg)?;
    let unpushed_commit_count = commands::mount::state::unpushed_commit_count(&state_dir);
    let dirty = !changes.is_empty();
    let branch_description_present = description_present(branch_description.as_deref());
    let needs_push = unpushed_commit_count > 0;
    // Mounts are always remote-backed: cfg.owner/cfg.repo/cfg.remote_url are always
    // Some, so a mount always has a configured remote.
    let branch_closed = branch_status.as_deref() == Some("closed");
    let can_finish = can_finish(
        &progress_state,
        branch.as_deref(),
        branch_closed,
        false,
        true,
        true,
    );
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(
        &progress_state,
        branch.as_deref(),
        branch_closed,
        false,
        true,
        true,
    );
    let refresh_errors: Vec<String> = refresh
        .then(|| "refresh is not supported inside mounts yet".to_string())
        .into_iter()
        .collect();
    let recommended_next_commands = mount_next_commands(
        finish_eligible,
        branch_closed,
        progress_state.in_progress,
        dirty,
        unpushed_commit_count,
        &progress_state.next_commands,
    );
    let recommended_action = recommended_action(
        WorkContext::Mount,
        &recommended_next_commands,
        &progress_state,
        dirty,
        false,
        needs_push,
        unpushed_commit_count,
        true,
        refresh,
        false,
        &refresh_errors,
        None,
        false,
        blocking_reason.as_deref(),
    );

    Ok(AgentWorkState {
        context: WorkContext::Mount,
        repo_owner: Some(cfg.owner.clone()),
        repo_name: Some(cfg.repo.clone()),
        remote_url: Some(cfg.remote_url.clone()),
        branch,
        branch_description,
        branch_description_present,
        head,
        local_parent_head: Some(cfg.base_commit.clone()),
        remote_parent_head: None,
        remote_parent_fetched_at: None,
        current_branch_pushed_head: None,
        current_branch_push_checked: false,
        current_branch_push_source: None,
        refresh_requested: refresh,
        refresh_supported: false,
        refresh_errors,
        needs_pull: false,
        needs_push,
        can_finish,
        dirty,
        changes,
        unpushed_commit_count,
        progress_state: progress_state.clone(),
        recommended_next_commands,
        recommended_action,
        blocking_reason,
        finish_eligible,
        mount: Some(serde_json::json!(commands::mount::mount_context_json(&cfg))),
    })
}

#[derive(Debug)]
pub struct CheckoutProgressState {
    pub merge_in_progress: bool,
    pub sync_in_progress: bool,
    pub json: output::ProgressStateJson,
}

pub fn checkout_progress_state(ctx: &crate::resolve::RepoContext) -> CheckoutProgressState {
    let merge_in_progress = ctx.oak_dir.join("MERGE_HEAD").exists();
    let sync_in_progress =
        ctx.oak_dir.join("SYNC_HEAD").exists() || ctx.work_tree.join(".oak/SYNC_HEAD").exists();
    let mut json = output::ProgressStateJson::default();
    if merge_in_progress {
        json.in_progress = true;
        json.kind = Some("merge".to_string());
        json.next_commands = vec![
            "oak merge --continue".to_string(),
            "oak merge --abort".to_string(),
        ];
    } else if sync_in_progress {
        json.in_progress = true;
        json.kind = Some("sync".to_string());
        json.conflict_paths = sync_conflict_paths(&ctx.work_tree);
        json.next_commands = vec![
            "oak pull --continue".to_string(),
            "oak pull --abort".to_string(),
        ];
    }
    CheckoutProgressState {
        merge_in_progress,
        sync_in_progress,
        json,
    }
}

/// Tier-1 diff-evidence commands for a checkout with local work: the
/// worktree diff when the tree is dirty, plus the committed-work branch
/// diff when the branch has commits. Both are named when both kinds of
/// work exist — a dirty tree must never hide committed work.
fn diff_evidence_commands(
    in_progress: bool,
    branch_closed: bool,
    dirty: bool,
    local_work_commit_count: usize,
    branch_name: Option<&str>,
) -> Vec<String> {
    let mut commands = Vec::new();
    if in_progress || branch_closed {
        return commands;
    }
    if dirty {
        commands.push("oak diff --json".to_string());
    }
    if local_work_commit_count > 0 {
        if let Some(branch) = branch_name {
            commands.push(format!("oak diff {branch} --json"));
        }
    }
    commands
}

#[allow(clippy::too_many_arguments)]
fn checkout_next_commands(
    finish_eligible: bool,
    branch_closed: bool,
    in_progress: bool,
    dirty: bool,
    needs_pull: bool,
    needs_push: bool,
    local_work_commit_count: usize,
    unpushed_commit_count: usize,
    remote_configured: bool,
    remote_auth_configured: bool,
    remote_url: Option<&str>,
    progress_next_commands: &[String],
) -> Vec<String> {
    if in_progress {
        progress_next_commands.to_vec()
    } else if branch_closed {
        vec!["oak status --json".to_string()]
    } else if remote_configured && !remote_auth_configured {
        let mut commands = vec![format!(
            "oak login -r {}",
            remote_url.unwrap_or("https://oak.space")
        )];
        if dirty {
            commands.push("oak commit".to_string());
        }
        commands
    } else if dirty && needs_pull {
        vec!["oak commit".to_string(), "oak pull".to_string()]
    } else if needs_pull {
        vec!["oak pull".to_string()]
    } else if finish_eligible
        && remote_configured
        && (dirty || needs_push || unpushed_commit_count > 0)
    {
        let mut commands = vec!["oak finish --desc-file <file> --json".to_string()];
        if dirty {
            commands.push("oak commit --push".to_string());
        }
        if needs_push || unpushed_commit_count > 0 {
            commands.push("oak push".to_string());
        }
        commands
    } else if dirty {
        vec!["oak commit".to_string()]
    } else if needs_push || unpushed_commit_count > 0 {
        vec![
            if remote_configured {
                "oak push".to_string()
            } else {
                commands::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string()
            },
            "oak merge".to_string(),
        ]
    } else if local_work_commit_count > 0 || finish_eligible {
        vec!["oak finish --desc-file <file> --json".to_string()]
    } else {
        vec!["oak status --json".to_string()]
    }
}

#[allow(clippy::too_many_arguments)]
fn recommended_action(
    context: WorkContext,
    recommended_next_commands: &[String],
    progress_state: &output::ProgressStateJson,
    dirty: bool,
    needs_pull: bool,
    needs_push: bool,
    unpushed_commit_count: usize,
    remote_configured: bool,
    refresh_requested: bool,
    refresh_supported: bool,
    refresh_errors: &[String],
    remote_parent_fetched_at: Option<&str>,
    current_branch_push_checked: bool,
    blocking_reason: Option<&str>,
) -> output::AgentRecommendedActionJson {
    agent_action::from_progress_command(
        match context {
            WorkContext::Checkout => AgentActionContext::Checkout,
            WorkContext::Mount => AgentActionContext::Mount,
        },
        recommended_next_commands,
        progress_state,
        AgentActionState {
            dirty,
            needs_pull,
            needs_push,
            unpushed_commit_count,
            progress_in_progress: progress_state.in_progress,
        },
        RemoteFreshnessInput {
            remote_configured,
            refresh_requested,
            refresh_supported,
            refresh_errors,
            remote_parent_fetched_at,
            current_branch_push_checked,
        },
        blocking_reason.map(ToString::to_string),
    )
}

fn refreshed_unpushed_commit_count(
    repo: &dyn oak_core::Repository,
    branch_name: Option<&str>,
    local_work_commit_count: usize,
    local_head: Option<&str>,
    current_branch_pushed_head: Option<&str>,
    current_branch_push_checked: bool,
) -> Result<usize> {
    if !current_branch_push_checked {
        return Ok(local_work_commit_count);
    }
    let Some(branch_name) = branch_name else {
        return Ok(0);
    };
    match (local_head, current_branch_pushed_head) {
        (Some(local), Some(pushed)) if local == pushed => Ok(0),
        (Some(_), Some(pushed)) => repo
            .get_commits_since(branch_name, Some(&oak_core::Hash(pushed.to_string())))
            .map(|commits| commits.len()),
        (Some(_), None) => Ok(local_work_commit_count),
        _ => Ok(0),
    }
}

fn mount_next_commands(
    finish_eligible: bool,
    branch_closed: bool,
    in_progress: bool,
    dirty: bool,
    unpushed_commit_count: usize,
    progress_next_commands: &[String],
) -> Vec<String> {
    if in_progress {
        progress_next_commands.to_vec()
    } else if branch_closed {
        vec!["oak status --json".to_string()]
    } else if finish_eligible && (dirty || unpushed_commit_count > 0) {
        let mut commands = vec!["oak finish --desc-file <file> --json".to_string()];
        if dirty {
            commands.push("oak commit".to_string());
        }
        if unpushed_commit_count > 0 {
            commands.push("oak push".to_string());
        }
        commands
    } else if dirty {
        vec!["oak commit".to_string(), "oak push".to_string()]
    } else if unpushed_commit_count > 0 {
        vec!["oak push".to_string()]
    } else if finish_eligible {
        vec!["oak finish --desc-file <file> --json".to_string()]
    } else {
        vec!["oak status --json".to_string()]
    }
}

fn description_present(description: Option<&str>) -> bool {
    description.is_some_and(|d| !d.trim().is_empty())
}

fn can_finish(
    progress_state: &output::ProgressStateJson,
    branch_name: Option<&str>,
    branch_closed: bool,
    needs_pull: bool,
    remote_configured: bool,
    remote_auth_configured: bool,
) -> bool {
    blocking_reason(
        progress_state,
        branch_name,
        branch_closed,
        needs_pull,
        remote_configured,
        remote_auth_configured,
    )
    .is_none()
}

fn blocking_reason(
    progress_state: &output::ProgressStateJson,
    branch_name: Option<&str>,
    branch_closed: bool,
    needs_pull: bool,
    remote_configured: bool,
    remote_auth_configured: bool,
) -> Option<String> {
    if progress_state.in_progress {
        return Some(
            progress_state
                .kind
                .as_deref()
                .unwrap_or("operation")
                .to_string()
                + "_in_progress",
        );
    }
    if branch_name.is_none() {
        return Some("no_current_branch".to_string());
    }
    if branch_closed {
        return Some("branch_closed".to_string());
    }
    // `oak finish` commits and then pushes; with no remote the push fails after the
    // commit has already mutated the branch. Block finish honestly when there is no
    // remote (a repo with no remote can't have needs_pull, so this check precedes it).
    if !remote_configured {
        return Some("no_remote_configured".to_string());
    }
    if !remote_auth_configured {
        return Some("auth_missing".to_string());
    }
    if needs_pull {
        return Some("needs_pull".to_string());
    }
    None
}

fn finish_auth_configured(repo: &dyn oak_core::Repository, remote: &str) -> bool {
    commands::credentials::effective_token(
        remote,
        repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
    )
    .is_some()
}

#[derive(Deserialize)]
struct SyncStateJson {
    #[serde(default)]
    conflict_paths: Vec<String>,
}

pub(crate) fn sync_conflict_paths(work_tree: &Path) -> Vec<String> {
    let path = work_tree.join(".oak/SYNC_STATE");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<SyncStateJson>(&raw)
        .map(|state| state.conflict_paths)
        .unwrap_or_default()
}

pub(crate) fn conflict_paths_for_path(path: &Path) -> Vec<String> {
    let Ok(ctx) = crate::resolve::resolve(path) else {
        return Vec::new();
    };
    if ctx.oak_dir.join("MERGE_HEAD").exists() {
        let Ok(ignore) = oak_core::IgnorePatterns::new(&ctx.work_tree) else {
            return Vec::new();
        };
        return crate::commands::merge::find_conflict_markers(
            &ctx.work_tree,
            &ctx.work_tree,
            &ignore,
        )
        .unwrap_or_default();
    }
    if ctx.oak_dir.join("SYNC_HEAD").exists() || ctx.work_tree.join(".oak/SYNC_HEAD").exists() {
        return sync_conflict_paths(&ctx.work_tree);
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::AgentRecommendedActionKindJson as ActionKind;

    #[test]
    fn diff_evidence_recommends_both_diffs_for_dirty_tree_with_commits() {
        // A dirty tree must not hide the branch's committed work: both the
        // worktree diff and the branch diff are tier-1 evidence.
        let commands = diff_evidence_commands(false, false, true, 2, Some("feature"));
        assert_eq!(commands, vec!["oak diff --json", "oak diff feature --json"]);
    }

    #[test]
    fn diff_evidence_recommends_single_diff_for_single_kind_of_work() {
        assert_eq!(
            diff_evidence_commands(false, false, true, 0, Some("feature")),
            vec!["oak diff --json"]
        );
        assert_eq!(
            diff_evidence_commands(false, false, false, 1, Some("feature")),
            vec!["oak diff feature --json"]
        );
        assert!(diff_evidence_commands(false, false, false, 0, Some("feature")).is_empty());
    }

    #[test]
    fn diff_evidence_stays_quiet_when_in_progress_or_closed() {
        assert!(diff_evidence_commands(true, false, true, 1, Some("feature")).is_empty());
        assert!(diff_evidence_commands(false, true, true, 1, Some("feature")).is_empty());
    }

    #[test]
    fn checkout_next_commands_recommend_pull_for_clean_needs_pull() {
        let commands = checkout_next_commands(
            false,
            false,
            false,
            false,
            true,
            false,
            0,
            0,
            true,
            true,
            None,
            &[],
        );

        assert_eq!(commands, vec!["oak pull"]);
    }

    #[test]
    fn checkout_next_commands_checkpoint_dirty_tree_before_pull() {
        let commands = checkout_next_commands(
            false,
            false,
            false,
            true,
            true,
            false,
            0,
            0,
            true,
            true,
            None,
            &[],
        );

        assert_eq!(commands, vec!["oak commit", "oak pull"]);
    }

    #[test]
    fn checkout_next_commands_inspect_only_for_closed_branch() {
        let commands = checkout_next_commands(
            false,
            true,
            false,
            false,
            false,
            true,
            1,
            1,
            true,
            true,
            None,
            &[],
        );

        assert_eq!(commands, vec!["oak status --json"]);
    }

    #[test]
    fn checkout_next_commands_preserve_progress_commands_for_closed_branch() {
        let progress_commands = vec![
            "oak merge --continue".to_string(),
            "oak merge --abort".to_string(),
        ];
        let commands = checkout_next_commands(
            false,
            true,
            true,
            false,
            false,
            true,
            1,
            1,
            true,
            true,
            None,
            &progress_commands,
        );

        assert_eq!(commands, progress_commands);
    }

    #[test]
    fn mount_next_commands_inspect_only_for_closed_branch() {
        let commands = mount_next_commands(false, true, false, false, 1, &[]);

        assert_eq!(commands, vec!["oak status --json"]);
    }

    #[test]
    fn mount_next_commands_preserve_progress_commands_for_closed_branch() {
        let progress_commands = vec![
            "oak pull --continue".to_string(),
            "oak pull --abort".to_string(),
        ];
        let commands = mount_next_commands(false, true, true, false, 1, &progress_commands);

        assert_eq!(commands, progress_commands);
    }

    #[test]
    fn recommended_action_marks_pull_as_network_mutation() {
        let progress = output::ProgressStateJson::default();
        let action = recommended_action(
            WorkContext::Checkout,
            &["oak pull".to_string()],
            &progress,
            false,
            true,
            false,
            0,
            true,
            true,
            true,
            &[],
            Some("123"),
            true,
            Some("needs_pull"),
        );

        assert_eq!(action.kind, ActionKind::Pull);
        assert_eq!(serde_json::to_value(action.kind).unwrap(), "pull");
        assert_eq!(action.command, "oak pull");
        assert!(action.mutates);
        assert!(action.needs_network);
        assert_eq!(action.confidence, "high");
        assert_eq!(action.remote_freshness, "fresh");
        assert_eq!(action.blocking_reason.as_deref(), Some("needs_pull"));
        assert!(action
            .risk_notes
            .iter()
            .any(|note| note == "pull_updates_working_tree"));
    }

    #[test]
    fn recommended_action_marks_truly_idle_status_as_noop() {
        let progress = output::ProgressStateJson::default();
        let action = recommended_action(
            WorkContext::Checkout,
            &["oak status --json".to_string()],
            &progress,
            false,
            false,
            false,
            0,
            true,
            true,
            true,
            &[],
            Some("123"),
            true,
            None,
        );

        assert_eq!(action.kind, ActionKind::Noop);
        assert_eq!(serde_json::to_value(action.kind).unwrap(), "noop");
        assert_eq!(action.command, "oak status --json");
        assert!(!action.mutates);
        assert!(!action.needs_network);
        assert_eq!(action.blocking_reason, None);
        assert!(action.risk_notes.is_empty());
    }

    #[test]
    fn recommended_action_keeps_unknown_freshness_idle_state_as_inspect() {
        let progress = output::ProgressStateJson::default();
        let action = recommended_action(
            WorkContext::Checkout,
            &["oak status --json".to_string()],
            &progress,
            false,
            false,
            false,
            0,
            true,
            false,
            true,
            &[],
            None,
            false,
            None,
        );

        assert_eq!(action.kind, ActionKind::Inspect);
        assert_eq!(serde_json::to_value(action.kind).unwrap(), "inspect");
        assert_eq!(action.remote_freshness, "unknown");
        assert_eq!(action.command, "oak status --json");
    }
}
