//! Shared agent-facing work state for regular checkouts and mounts.

use std::path::Path;

use oak_core::{MetadataKey, Result, SqliteRepository};
use serde::Deserialize;

use crate::commands;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
/// Full `oak agent state --json` v2 removes the redundant top-level
/// `can_finish` alias. Consumers should use `finish_eligible`.
pub const AGENT_STATE_SCHEMA_VERSION: u32 = 2;
/// Compact agent-state JSON remains v1 because it never emitted `can_finish`.
pub const AGENT_STATE_COMPACT_SCHEMA_VERSION: u32 = 1;

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
    let mut current_branch_pushed_head = None;
    let mut current_branch_push_checked = false;
    let mut refresh_errors = Vec::new();
    if refresh {
        if branch
            .as_ref()
            .and_then(|b| b.parent_branch.as_deref())
            .is_some_and(|parent| parent == oak_core::DEFAULT_BRANCH)
        {
            output::begin_capture();
            let fetched =
                commands::sync::fetch_parent_from_server(repo.as_ref(), oak_core::DEFAULT_BRANCH)
                    .await;
            let _captured = output::end_capture();
            match fetched {
                Ok(head) => remote_parent_head = head.map(|h| h.to_string()),
                Err(err) => refresh_errors.push(format!("remote_parent_head: {err}")),
            }
        }

        if let (Some(remote), Some(owner), Some(repo_name), Some(branch_name)) = (
            repo.get_metadata(MetadataKey::RemoteUrl)?,
            repo.get_metadata(MetadataKey::RepoOwner)?,
            repo.get_metadata(MetadataKey::RepoName)?,
            branch_name.as_deref(),
        ) {
            let remote = remote.trim_end_matches('/').to_string();
            let token = std::env::var("OAK_API_KEY")
                .ok()
                .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
                .or_else(|| commands::credentials::get_token_for_server(&remote));
            let fetched = commands::push::fetch_remote_branch_head(
                &crate::http::api_client(),
                &remote,
                &format!("{owner}/{repo_name}"),
                branch_name,
                token.as_deref(),
            )
            .await;
            match fetched {
                Ok(head) => {
                    current_branch_push_checked = true;
                    current_branch_pushed_head = head.map(|h| h.to_string());
                }
                Err(err) => {
                    current_branch_push_checked = false;
                    refresh_errors.push(format!("current_branch_pushed_head: {err}"));
                }
            }
        }
    }
    let repo_owner = repo.get_metadata(MetadataKey::RepoOwner)?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?;
    let remote_url = repo.get_metadata(MetadataKey::RemoteUrl)?;
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
        current_branch_push_checked,
    )?;
    let needs_push = if current_branch_push_checked {
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
        needs_pull,
        remote_configured,
        remote_auth_configured,
    );
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(
        &progress.json,
        branch_name.as_deref(),
        needs_pull,
        remote_configured,
        remote_auth_configured,
    );
    let recommended_next_commands = checkout_next_commands(
        can_finish,
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
    let (branch, branch_description, _parent, head, _branch_status) =
        commands::mount::branch_snapshot(&cache, &cfg)?;
    let unpushed_commit_count = commands::mount::state::unpushed_commit_count(&state_dir);
    let dirty = !changes.is_empty();
    let branch_description_present = description_present(branch_description.as_deref());
    let needs_push = unpushed_commit_count > 0;
    // Mounts are always remote-backed: cfg.owner/cfg.repo/cfg.remote_url are always
    // Some, so a mount always has a configured remote.
    let can_finish = can_finish(&progress_state, branch.as_deref(), false, true, true);
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(&progress_state, branch.as_deref(), false, true, true);
    let refresh_errors: Vec<String> = refresh
        .then(|| "refresh is not supported inside mounts yet".to_string())
        .into_iter()
        .collect();
    let recommended_next_commands = mount_next_commands(
        finish_eligible,
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

#[allow(clippy::too_many_arguments)]
fn checkout_next_commands(
    finish_eligible: bool,
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
        vec!["oak push".to_string(), "oak merge".to_string()]
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
    let mut command = recommended_next_commands
        .first()
        .cloned()
        .unwrap_or_else(|| "oak status --json".to_string());
    if !remote_configured && command == "oak push" {
        command = "oak push --repo <org>/<repo>".to_string();
    }
    let kind = if progress_state.in_progress {
        "resolve_conflict"
    } else {
        action_kind_for_command(&command)
    };
    let remote_freshness = remote_freshness(
        remote_configured,
        refresh_requested,
        refresh_supported,
        refresh_errors,
        remote_parent_fetched_at,
        current_branch_push_checked,
    );
    let risk_notes = action_risk_notes(
        kind,
        context,
        dirty,
        needs_pull,
        needs_push,
        unpushed_commit_count,
        remote_configured,
        refresh_errors,
        progress_state,
    );
    let needs_network = action_needs_network(kind);
    let confidence = action_confidence(
        needs_network,
        context,
        refresh_requested,
        refresh_errors,
        &remote_freshness,
    );

    output::AgentRecommendedActionJson {
        kind: kind.to_string(),
        command,
        mutates: action_mutates(kind),
        needs_network,
        confidence: confidence.to_string(),
        remote_freshness,
        blocking_reason: blocking_reason.map(ToString::to_string),
        risk_notes,
    }
}

fn action_kind_for_command(command: &str) -> &'static str {
    match command {
        "oak commit" => "commit",
        "oak commit --push" => "commit_push",
        "oak push" => "push",
        _ if command.starts_with("oak push ") => "push",
        "oak pull" | "oak pull --continue" | "oak pull --abort" => "pull",
        "oak merge" | "oak merge --continue" | "oak merge --abort" => "merge",
        "oak status --json" | "oak agent state --json" | "oak status --porcelain" => "inspect",
        _ if command.starts_with("oak finish ") => "finish",
        _ if command.starts_with("oak close ") => "close_branch",
        _ if command.starts_with("oak branch review ") => "review_branch",
        _ => "inspect",
    }
}

fn action_mutates(kind: &str) -> bool {
    !matches!(kind, "inspect" | "review_branch" | "noop")
}

fn action_needs_network(kind: &str) -> bool {
    matches!(
        kind,
        "commit_push" | "push" | "finish" | "pull" | "close_branch" | "review_branch"
    )
}

fn remote_freshness(
    remote_configured: bool,
    refresh_requested: bool,
    refresh_supported: bool,
    refresh_errors: &[String],
    remote_parent_fetched_at: Option<&str>,
    current_branch_push_checked: bool,
) -> String {
    if !remote_configured {
        "not_configured".to_string()
    } else if !refresh_supported {
        "unsupported".to_string()
    } else if refresh_requested && refresh_errors.is_empty() {
        "fresh".to_string()
    } else if refresh_requested {
        "degraded".to_string()
    } else if current_branch_push_checked {
        "fresh".to_string()
    } else if remote_parent_fetched_at.is_some() {
        "cached".to_string()
    } else {
        "unknown".to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn action_risk_notes(
    kind: &str,
    context: WorkContext,
    dirty: bool,
    needs_pull: bool,
    needs_push: bool,
    unpushed_commit_count: usize,
    remote_configured: bool,
    refresh_errors: &[String],
    progress_state: &output::ProgressStateJson,
) -> Vec<String> {
    let mut notes = Vec::new();
    if progress_state.in_progress {
        notes.push("operation_in_progress".to_string());
    }
    if !remote_configured {
        notes.push("no_remote_configured".to_string());
    }
    if !refresh_errors.is_empty() {
        notes.push("remote_refresh_degraded".to_string());
    }
    if kind == "commit" {
        notes.push("commit_is_local_checkpoint_only".to_string());
    }
    if kind == "commit_push" {
        notes.push("commit_push_contacts_remote".to_string());
    }
    if kind == "finish" && dirty {
        notes.push("finish_will_commit_and_push".to_string());
    }
    if kind == "pull" {
        notes.push("pull_updates_working_tree".to_string());
    }
    if needs_pull && kind != "pull" {
        notes.push("pull_required_after_checkpoint".to_string());
    }
    if (needs_push || unpushed_commit_count > 0)
        && !matches!(kind, "push" | "finish" | "commit_push")
    {
        notes.push("unpushed_commits_remain".to_string());
    }
    if context == WorkContext::Mount && matches!(kind, "finish" | "push") {
        notes.push("mount_state_will_change".to_string());
    }
    notes
}

fn action_confidence(
    needs_network: bool,
    context: WorkContext,
    refresh_requested: bool,
    refresh_errors: &[String],
    remote_freshness: &str,
) -> &'static str {
    if !refresh_errors.is_empty() {
        "low"
    } else if needs_network
        && context == WorkContext::Checkout
        && !refresh_requested
        && !matches!(remote_freshness, "fresh")
    {
        "medium"
    } else {
        "high"
    }
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
    in_progress: bool,
    dirty: bool,
    unpushed_commit_count: usize,
    progress_next_commands: &[String],
) -> Vec<String> {
    if in_progress {
        progress_next_commands.to_vec()
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
    needs_pull: bool,
    remote_configured: bool,
    remote_auth_configured: bool,
) -> bool {
    blocking_reason(
        progress_state,
        branch_name,
        needs_pull,
        remote_configured,
        remote_auth_configured,
    )
    .is_none()
}

fn blocking_reason(
    progress_state: &output::ProgressStateJson,
    branch_name: Option<&str>,
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
    std::env::var("OAK_API_KEY").is_ok()
        || repo
            .get_metadata(MetadataKey::ApiKey)
            .ok()
            .flatten()
            .is_some()
        || commands::credentials::get_token_for_server(remote).is_some()
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

    #[test]
    fn checkout_next_commands_recommend_pull_for_clean_needs_pull() {
        let commands = checkout_next_commands(
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
        let commands =
            checkout_next_commands(false, false, true, true, false, 0, 0, true, true, None, &[]);

        assert_eq!(commands, vec!["oak commit", "oak pull"]);
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

        assert_eq!(action.kind, "pull");
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
}
