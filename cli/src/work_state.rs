//! Shared agent-facing work state for regular checkouts and mounts.

use std::path::Path;

use oak_core::{MetadataKey, Result, SqliteRepository};
use serde::Deserialize;

use crate::commands;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;

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
    pub blocking_reason: Option<String>,
    pub finish_eligible: bool,
    pub mount: Option<serde_json::Value>,
}

impl From<AgentWorkState> for output::AgentStateJson {
    fn from(state: AgentWorkState) -> Self {
        output::AgentStateJson {
            schema_version: SCHEMA_VERSION,
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
            can_finish: state.can_finish,
            dirty: state.dirty,
            changes: state.changes,
            unpushed_commit_count: state.unpushed_commit_count,
            progress_state: state.progress_state,
            recommended_next_commands: state.recommended_next_commands,
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
    );
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(
        &progress.json,
        branch_name.as_deref(),
        needs_pull,
        remote_configured,
    );
    let recommended_next_commands = checkout_next_commands(
        can_finish,
        progress.json.in_progress,
        dirty,
        needs_push,
        local_work_commit_count,
        unpushed_commit_count,
        remote_configured,
        &progress.json.next_commands,
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
    let can_finish = can_finish(&progress_state, branch.as_deref(), false, true);
    let finish_eligible = can_finish;
    let blocking_reason = blocking_reason(&progress_state, branch.as_deref(), false, true);

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
        refresh_errors: refresh
            .then(|| "refresh is not supported inside mounts yet".to_string())
            .into_iter()
            .collect(),
        needs_pull: false,
        needs_push,
        can_finish,
        dirty,
        changes,
        unpushed_commit_count,
        progress_state: progress_state.clone(),
        recommended_next_commands: mount_next_commands(
            finish_eligible,
            progress_state.in_progress,
            dirty,
            unpushed_commit_count,
            &progress_state.next_commands,
        ),
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

fn checkout_next_commands(
    finish_eligible: bool,
    in_progress: bool,
    dirty: bool,
    needs_push: bool,
    local_work_commit_count: usize,
    unpushed_commit_count: usize,
    remote_configured: bool,
    progress_next_commands: &[String],
) -> Vec<String> {
    if in_progress {
        progress_next_commands.to_vec()
    } else if finish_eligible
        && remote_configured
        && (dirty || needs_push || unpushed_commit_count > 0)
    {
        let mut commands = vec!["oak finish --desc-file <file> --json".to_string()];
        if dirty {
            commands.push("oak commit".to_string());
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
) -> bool {
    blocking_reason(progress_state, branch_name, needs_pull, remote_configured).is_none()
}

fn blocking_reason(
    progress_state: &output::ProgressStateJson,
    branch_name: Option<&str>,
    needs_pull: bool,
    remote_configured: bool,
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
    if needs_pull {
        return Some("needs_pull".to_string());
    }
    None
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
