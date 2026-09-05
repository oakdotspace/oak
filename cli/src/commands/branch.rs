use std::path::{Path, PathBuf};

use oak_core::{
    Branch, BranchStatus, CloseReason, MetadataKey, OakError, Repository, Result, SqliteRepository,
};
use serde::{Deserialize, Serialize};

use crate::output;

/// Create a new branch.
///
/// Default behavior: parent is the current branch; the branch is seeded
/// at no specific commit (its effective head walks up to the parent).
///
/// With `from`: seed the branch at the given commit (full hash or short
/// prefix), update the working tree to that commit's manifest, and set
/// HEAD. This is the recovery path for "I need a new branch pinned to a
/// specific commit" — e.g. when local state has drifted from the server
/// and a closed branch is in the way.
///
/// With `parent_override`: pin the parent branch explicitly instead of
/// using the current branch. Useful when seeding from a commit while
/// detached, or when you want a personal branch parented onto `main`
/// regardless of where you currently are.
pub fn new_branch(
    path: &Path,
    name: &str,
    description: Option<&str>,
    from: Option<&str>,
    parent_override: Option<&str>,
) -> Result<()> {
    validate_branch_name(name)?;
    let ctx = crate::resolve::resolve(path)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    if repo.get_branch(name)?.is_some() {
        return Err(OakError::BranchAlreadyExists(name.to_string()));
    }

    // Resolve the seed commit (if any) up front so we fail before mutating
    // state.
    let seed_hash = match from {
        Some(reference) => Some(super::checkout::resolve_commit_ref(
            repo.as_ref(),
            reference,
        )?),
        None => None,
    };

    // Refuse to clobber uncommitted work when we'd be rewriting the tree.
    if seed_hash.is_some() {
        let (changes, _, _) = super::commit::get_status(path)?;
        if !changes.is_empty() {
            return Err(OakError::UncommittedChanges);
        }
    }

    // Flat branching model: every branch is parented onto the trunk
    // (`main`), never onto another feature branch. We no longer inherit the
    // current branch as the parent — that's what produced confusing stacked
    // branch-on-branch relationships. An explicit `--parent` is only honored
    // when it names the trunk (or is empty); anything else is rejected.
    if let Some(p) = parent_override {
        if !p.is_empty() && p != oak_core::DEFAULT_BRANCH {
            return Err(OakError::InvalidArgument(format!(
                "branches can only be parented onto the trunk ('{trunk}'); \
                 parenting onto another branch ('{p}') isn't allowed — Oak uses \
                 a flat branch-per-task model where every branch merges back \
                 into '{trunk}'",
                trunk = oak_core::DEFAULT_BRANCH,
            )));
        }
    }
    let parent = Some(oak_core::DEFAULT_BRANCH.to_string());

    let branch = Branch::new(
        name.to_string(),
        description.map(|d| d.to_string()),
        parent.clone(),
    );
    repo.store_branch(&branch)?;

    if let Some(ref hash) = seed_hash {
        let commit = repo
            .get_commit(hash)?
            .ok_or_else(|| OakError::CommitNotFound(hash.to_string()))?;
        let manifest = repo
            .get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(oak_core::Manifest::empty);

        let lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
        super::switch::update_working_dir(&lock, root, repo.as_ref(), &manifest)?;
        repo.set_branch_head(name, hash)?;
        repo.set_head(hash)?;
    }

    repo.set_current_branch(name)?;

    let parent_msg = match parent {
        Some(ref p) => format!(" (parent: '{p}')"),
        None => String::new(),
    };
    match seed_hash {
        Some(h) => output::success(&format!(
            "Created branch '{name}'{parent_msg} at {}",
            h.short()
        )),
        None => output::success(&format!("Created branch '{name}'{parent_msg}")),
    }

    Ok(())
}

pub(crate) fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim() != name {
        return Err(OakError::InvalidArgument(
            "branch name cannot be empty or have leading/trailing whitespace".to_string(),
        ));
    }
    if name == oak_core::DEFAULT_BRANCH {
        return Err(OakError::InvalidArgument(
            "`main` only exists on the server; create a personal/feature branch instead"
                .to_string(),
        ));
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
    {
        return Err(OakError::InvalidArgument(
            "branch name cannot contain whitespace, control characters, or path separators"
                .to_string(),
        ));
    }
    Ok(())
}

/// List all branches in the repository
pub fn list_branches(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let branches = repo.list_branches()?;
    let current = repo.get_current_branch_name()?;

    if branches.is_empty() {
        output::info("No branches");
        return Ok(());
    }

    for br in &branches {
        let is_current = current.as_deref() == Some(&br.name);
        let marker = if is_current { "* " } else { "  " };

        let status_color = match br.status {
            BranchStatus::Open => output::colors::GREEN,
            BranchStatus::Closed => output::colors::DIM,
        };
        let status_str = format!("{}{}{}", status_color, br.status, output::colors::RESET);

        let name_display = if is_current {
            format!(
                "{}{}{}{}",
                output::colors::GREEN,
                output::colors::BOLD,
                br.name,
                output::colors::RESET
            )
        } else {
            br.name.clone()
        };

        let mut line = format!("{marker}{name_display} [{status_str}]");

        if let Some(ref desc) = br.description {
            line.push_str(&format!(" - {desc}"));
        }

        if let Some(ref parent) = br.parent_branch {
            line.push_str(&format!(
                " {}(parent: {}){}",
                output::colors::DIM,
                parent,
                output::colors::RESET
            ));
        }

        // Show head hash if present
        if let Ok(Some(head)) = repo.get_branch_head(&br.name) {
            line.push_str(&format!(
                " {}[head: {}]{}",
                output::colors::CYAN,
                head.short(),
                output::colors::RESET
            ));
        }

        output::print_line(&line);
    }

    Ok(())
}

pub fn show_current_branch(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    output::print_line(repo.get_current_branch_name()?.as_deref().unwrap_or(""));
    Ok(())
}

/// List all branches as one JSON document on stdout.
pub fn list_branches_json(path: &Path, status: Option<&str>) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let mut branches = branch_json_rows(repo.as_ref())?;
    if let Some(status) = status {
        branches.retain(|branch| branch.status == status);
    }
    output::print_json(&branches)
}

/// List branch metadata directly from the configured remote without switching
/// or materializing any worktree files.
pub async fn list_remote_branches_json(path: &Path, status: Option<&str>) -> Result<()> {
    let remote = RemoteWorkspace::open(path)?.remote;
    let mut branches = fetch_remote_branches(&remote).await?;
    if let Some(status) = status {
        branches.retain(|branch| branch.status == status);
    }
    output::print_json(
        &branches
            .into_iter()
            .map(RemoteBranchJson::from)
            .collect::<Vec<_>>(),
    )
}

/// Close a branch, preventing further commits.
///
/// If the repo is linked to a remote, best-effort sync the branch row so the
/// server sees the status-only change even when there are no commits to push.
pub async fn close_branch(
    path: &Path,
    name: &str,
    close_reason: Option<CloseReason>,
) -> Result<()> {
    close_branch_inner(path, name, close_reason, false).await
}

/// Close a local branch and emit machine-readable JSON.
pub async fn close_branch_json(
    path: &Path,
    name: &str,
    close_reason: Option<CloseReason>,
) -> Result<()> {
    close_branch_inner(path, name, close_reason, true).await
}

async fn close_branch_inner(
    path: &Path,
    name: &str,
    close_reason: Option<CloseReason>,
    json: bool,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let _br = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;

    repo.close_branch(name, close_reason.clone())?;

    if json {
        output::print_json(&close_branch_json_payload(
            name,
            close_reason.clone(),
            false,
            repo.get_current_branch_name()?,
        ))?;
    } else {
        output::success(&format!("Closed branch '{name}'"));
    }

    sync_branch_metadata_to_server(
        repo.as_ref(),
        name,
        "Branch status synced to server",
        "couldn't sync branch status to server",
        !json,
    )
    .await;

    Ok(())
}

/// Close a branch on the configured remote without switching to it.
///
/// This mirrors the remote branch row into local metadata so the existing
/// metadata-only push path can sync the closed status. It intentionally does
/// not set the current branch, HEAD, or materialize files.
pub async fn close_remote_branch_json(
    path: &Path,
    name: &str,
    close_reason: Option<CloseReason>,
) -> Result<()> {
    let workspace = RemoteWorkspace::open(path)?;
    let repo = workspace.repo;
    let remote = workspace.remote;
    let mut branches = fetch_remote_branches(&remote).await?;
    store_remote_branch_metadata(
        &repo,
        &mut branches,
        name,
        Some(BranchStatus::Closed),
        close_reason.clone(),
    )?;

    push_branch_metadata(&repo, &remote, name, "couldn't close remote branch").await?;

    output::print_json(&close_branch_json_payload(
        name,
        close_reason.clone(),
        true,
        repo.get_current_branch_name()?,
    ))
}

/// Show detailed information about a branch
pub fn show_branch(path: &Path, name: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let br = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;

    let current = repo.get_current_branch_name()?;
    let is_current = current.as_deref() == Some(name);

    output::header(&br.name);

    let status_color = match br.status {
        BranchStatus::Open => output::colors::GREEN,
        BranchStatus::Closed => output::colors::DIM,
    };
    let status_display = format!(
        "{}{}{}{}",
        status_color,
        output::colors::BOLD,
        br.status,
        output::colors::RESET
    );
    output::detail("status:", &status_display);

    if let Some(ref desc) = br.description {
        output::detail("description:", desc);
    }

    if let Some(ref parent) = br.parent_branch {
        output::detail("parent:", parent);
    }

    if br.status == BranchStatus::Closed {
        if let Some(reason) = br.close_reason {
            output::detail("close reason:", reason.as_str());
        }
    }

    output::detail(
        "created:",
        &br.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    );

    if is_current {
        output::detail(
            "",
            &format!(
                "{}{}current branch{}",
                output::colors::GREEN,
                output::colors::BOLD,
                output::colors::RESET,
            ),
        );
    }

    // Show head info
    if let Some(head_hash) = repo.get_branch_head(name)? {
        output::detail(
            "head:",
            &format!(
                "{}{}{}",
                output::colors::CYAN,
                head_hash.short(),
                output::colors::RESET,
            ),
        );
    } else {
        output::detail(
            "head:",
            &format!("{}(none){}", output::colors::DIM, output::colors::RESET,),
        );
    }

    // Show commits
    let commits = repo.get_commits_for_branch(name)?;
    if commits.is_empty() {
        output::blank();
        output::print_line(&format!(
            "{}No commits in this branch{}",
            output::colors::DIM,
            output::colors::RESET,
        ));
    } else {
        output::blank();
        output::header(&format!("Commits ({})", commits.len()));
        for commit in &commits {
            output::item(&format!(
                "{}{}{} {} {}{}{}",
                output::colors::CYAN,
                commit.hash.short(),
                output::colors::RESET,
                commit.timestamp.format("%Y-%m-%d %H:%M"),
                output::colors::DIM,
                commit.message.as_deref().unwrap_or(""),
                output::colors::RESET,
            ));
            for file in &commit.files {
                output::print_line(&format!(
                    "    {}",
                    output::format_change_type(file.change_type, &file.path)
                ));
            }
        }
    }

    Ok(())
}

/// Show one branch as JSON.
pub fn show_branch_json(path: &Path, name: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
    let current = repo.get_current_branch_name()?;
    let branch = branch_json_row(repo.as_ref(), branch, current.as_deref())?;
    output::print_json(&branch)
}

/// Show one branch from the configured remote without switching to it.
pub async fn show_remote_branch_json(path: &Path, name: &str) -> Result<()> {
    let remote = RemoteWorkspace::open(path)?.remote;
    let branches = fetch_remote_branches(&remote).await?;
    let branch = branches
        .into_iter()
        .find(|branch| branch.name == name)
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
    output::print_json(&RemoteBranchJson::from(branch))
}

fn branch_json_rows(repo: &dyn oak_core::Repository) -> Result<Vec<output::BranchJson>> {
    let branches = repo.list_branches()?;
    let current = repo.get_current_branch_name()?;

    branches
        .into_iter()
        .map(|br| branch_json_row(repo, br, current.as_deref()))
        .collect()
}

fn branch_json_row(
    repo: &dyn oak_core::Repository,
    branch: Branch,
    current: Option<&str>,
) -> Result<output::BranchJson> {
    let head = repo.get_branch_head(&branch.name)?.map(|h| h.to_string());
    Ok(output::BranchJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        current: current == Some(branch.name.as_str()),
        name: branch.name,
        head,
        description: branch.description,
        status: branch.status.as_str().to_string(),
        close_reason: branch.close_reason.as_ref().map(|r| r.as_str().to_string()),
        created_at: branch.created_at.to_rfc3339(),
    })
}

fn close_branch_json_payload(
    name: &str,
    close_reason: Option<CloseReason>,
    remote: bool,
    current_branch: Option<String>,
) -> serde_json::Value {
    let list_cmd = if remote {
        "oak branch list --remote --status open --json"
    } else {
        "oak branch list --status closed --json"
    };
    serde_json::json!({
        "schema_version": crate::work_state::SCHEMA_VERSION,
        "branch": name,
        "status": "closed",
        "close_reason": close_reason.as_ref().map(|r| r.as_str()),
        "remote": remote,
        "current_branch": current_branch,
        "recommended_next_commands": [list_cmd]
    })
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteIdentity {
    pub remote_url: String,
    pub owner: String,
    pub repo_name: String,
    pub token: Option<String>,
}

pub(crate) struct RemoteWorkspace {
    pub repo: SqliteRepository,
    pub remote: RemoteIdentity,
    pub work_tree: PathBuf,
}

impl RemoteWorkspace {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            if let Some(dest) = super::mount::mount_dest_for(path)? {
                let (cfg, state_dir) = super::mount::config_for_dest(&dest)?;
                let repo = SqliteRepository::open_relaxed(&super::mount::state::cache_db_path(
                    &state_dir,
                ))?;
                let remote = RemoteIdentity::from_mount(&cfg, &state_dir);
                return Ok(Self {
                    repo,
                    remote,
                    work_tree: dest,
                });
            }
        }

        let ctx = crate::resolve::resolve(path)?;
        let repo = SqliteRepository::open(&ctx.db_path()?)?;
        let remote = RemoteIdentity::from_repo(&repo)?;
        Ok(Self {
            repo,
            remote,
            work_tree: ctx.work_tree,
        })
    }
}

impl RemoteIdentity {
    pub(crate) fn from_repo(repo: &dyn Repository) -> Result<Self> {
        let remote_url = repo
            .get_metadata(MetadataKey::RemoteUrl)?
            .ok_or_else(|| OakError::Server("Repository has no remote configured.".to_string()))?;
        let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
            OakError::Server("Repository metadata is missing remote owner.".to_string())
        })?;
        let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
            OakError::Server("Repository metadata is missing remote repo name.".to_string())
        })?;
        let token = super::credentials::effective_token(
            &remote_url,
            repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
        );
        Ok(Self {
            remote_url: remote_url.trim_end_matches('/').to_string(),
            owner,
            repo_name,
            token,
        })
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn from_mount(cfg: &super::mount::state::MountConfig, state_dir: &Path) -> Self {
        let repository_token =
            SqliteRepository::open_relaxed(&super::mount::state::cache_db_path(state_dir))
                .ok()
                .and_then(|cache| cache.get_metadata(MetadataKey::ApiKey).ok().flatten())
                .filter(|token| !token.trim().is_empty());
        let token = super::credentials::effective_token(&cfg.remote_url, repository_token);
        Self {
            remote_url: cfg.remote_url.trim_end_matches('/').to_string(),
            owner: cfg.owner.clone(),
            repo_name: cfg.repo.clone(),
            token,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RemoteBranchData {
    pub name: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parent_branch: Option<String>,
    #[serde(default = "default_branch_status")]
    pub status: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub close_reason: Option<String>,
}

fn default_branch_status() -> String {
    "open".to_string()
}

#[derive(Deserialize)]
struct RemoteBranchList {
    branches: Vec<RemoteBranchData>,
}

#[derive(Debug, Serialize)]
struct RemoteBranchJson {
    schema_version: u32,
    name: String,
    head: Option<String>,
    description: Option<String>,
    parent_branch: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_reason: Option<String>,
    created_at: Option<String>,
    current: bool,
    remote: bool,
}

impl From<RemoteBranchData> for RemoteBranchJson {
    fn from(branch: RemoteBranchData) -> Self {
        Self {
            schema_version: crate::work_state::SCHEMA_VERSION,
            name: branch.name,
            head: branch.head,
            description: branch.description,
            parent_branch: branch.parent_branch,
            status: branch.status,
            close_reason: branch.close_reason,
            created_at: branch.created_at,
            current: false,
            remote: true,
        }
    }
}

pub(crate) async fn fetch_remote_branches(
    remote: &RemoteIdentity,
) -> Result<Vec<RemoteBranchData>> {
    let client = crate::http::api_client();
    let url = format!(
        "{}/api/{}/{}/branches",
        remote.remote_url, remote.owner, remote.repo_name
    );
    let mut req = client.get(&url);
    if let Some(token) = remote.token.as_deref() {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    let list: RemoteBranchList = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    Ok(list.branches)
}

pub(crate) fn store_remote_branch_metadata(
    repo: &SqliteRepository,
    branches: &mut [RemoteBranchData],
    branch_name: &str,
    status_override: Option<BranchStatus>,
    close_reason_override: Option<CloseReason>,
) -> Result<()> {
    branches.sort_by_key(|branch| {
        if branch.name == branch_name {
            1
        } else if branch.name == oak_core::DEFAULT_BRANCH {
            0
        } else {
            2
        }
    });

    for branch in branches {
        if branch.name != branch_name && branch.name != oak_core::DEFAULT_BRANCH {
            continue;
        }
        let status = if branch.name == branch_name {
            status_override.unwrap_or_else(|| BranchStatus::from_db_str(&branch.status))
        } else {
            BranchStatus::from_db_str(&branch.status)
        };
        let created_at = branch
            .created_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let parent_branch = if branch.parent_branch.as_deref() == Some(&branch.name) {
            None
        } else {
            branch.parent_branch.clone()
        };
        let close_reason = if branch.name == branch_name {
            close_reason_override.clone().or_else(|| {
                branch
                    .close_reason
                    .as_deref()
                    .map(CloseReason::parse)
                    .transpose()
                    .ok()
                    .flatten()
            })
        } else {
            branch
                .close_reason
                .as_deref()
                .map(CloseReason::parse)
                .transpose()
                .ok()
                .flatten()
        };
        repo.upsert_branch_metadata(&Branch {
            name: branch.name.clone(),
            description: branch.description.clone(),
            parent_branch,
            status,
            close_reason,
            created_at,
        })?;
        if let Some(head) = branch.head.as_deref() {
            if let Ok(hash) = oak_core::Hash::from_hex(head) {
                if repo.get_commit(&hash)?.is_some() {
                    repo.set_branch_head(&branch.name, &hash)?;
                }
            }
        }
    }

    if repo.get_branch(branch_name)?.is_none() {
        return Err(OakError::BranchNotFound(branch_name.to_string()));
    }
    if status_override == Some(BranchStatus::Closed) {
        repo.close_branch(branch_name, close_reason_override)?;
    }
    Ok(())
}

async fn push_branch_metadata(
    repo: &dyn Repository,
    remote: &RemoteIdentity,
    branch_name: &str,
    error_action: &str,
) -> Result<()> {
    super::push::push_branch_metadata(
        repo,
        &remote.remote_url,
        &remote.owner,
        &remote.repo_name,
        branch_name,
        remote.token.as_deref(),
    )
    .await
    .map_err(|e| OakError::Server(format!("{error_action}: {e}")))?;
    Ok(())
}

/// Edit a branch's description
pub fn edit_branch(path: &Path, name: &str, description: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    // Verify the branch exists
    repo.get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;

    repo.update_branch_description(name, description)?;

    output::success(&format!("Updated description for branch '{name}'"));

    Ok(())
}

/// Edit the current branch's description.
pub async fn edit_current_branch(path: &Path, description: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let name = repo.get_current_branch_name()?.ok_or_else(|| {
        OakError::Io(std::io::Error::other(
            "No current branch set; switch to a branch before setting a description",
        ))
    })?;
    if name.is_empty() {
        return Err(OakError::Io(std::io::Error::other(
            "HEAD is detached; switch to a branch before setting a description",
        )));
    }

    repo.get_branch(&name)?
        .ok_or_else(|| OakError::BranchNotFound(name.clone()))?;

    repo.update_branch_description(&name, description)?;

    output::success(&format!("Updated description for branch '{name}'"));

    sync_branch_metadata_to_server(
        repo.as_ref(),
        &name,
        "Description synced to server",
        "couldn't sync to server",
        true,
    )
    .await;

    Ok(())
}

/// Best-effort sync of a branch's row to the server. Without this, metadata
/// set after the last push never leaves the machine: a commit-less `oak push`
/// returns "Already up to date" before sending branch metadata. No-op when
/// the repo has no remote configured; warns (never errors) on network/auth
/// failure so the local update always sticks.
async fn sync_branch_metadata_to_server(
    repo: &dyn oak_core::Repository,
    branch_name: &str,
    success_message: &str,
    warning_action: &str,
    emit_success: bool,
) {
    let Ok(Some(remote)) = repo.get_metadata(MetadataKey::RemoteUrl) else {
        return;
    };
    let Ok(Some(owner)) = repo.get_metadata(MetadataKey::RepoOwner) else {
        return;
    };
    let Ok(Some(repo_name)) = repo.get_metadata(MetadataKey::RepoName) else {
        return;
    };
    let token = super::credentials::effective_token(
        &remote,
        repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
    );

    match super::push::push_branch_metadata(
        repo,
        &remote,
        &owner,
        &repo_name,
        branch_name,
        token.as_deref(),
    )
    .await
    {
        Ok(()) if emit_success => output::success(success_message),
        Ok(()) => {}
        Err(e) => output::warning(&format!(
            "Saved locally but {warning_action}: {e}. Run `oak push` to retry."
        )),
    }
}

#[derive(Deserialize)]
struct RenameBranchResponse {
    id: i64,
    #[allow(dead_code)]
    old_name: String,
    #[allow(dead_code)]
    new_name: String,
    #[allow(dead_code)]
    renamed_at: String,
}

/// Rename a branch. If a remote is configured and the branch exists there,
/// perform the server-side rename first (so other clones discover it on
/// their next pull); only then mirror the rename locally and advance the
/// LastRenameId watermark so we don't replay our own rename. A fresh
/// clone's generated personal branch may be local-only until first push:
/// when the server specifically says that old name is absent, keep the
/// rename local and let a later `oak push` publish it.
pub fn rename_branch(path: &Path, old_name: &str, new_name: &str) -> Result<()> {
    validate_rename_target_name(new_name)?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    if old_name == new_name {
        output::info(&format!("Branch '{old_name}' already has that name"));
        return Ok(());
    }

    repo.get_branch(old_name)?
        .ok_or_else(|| OakError::BranchNotFound(old_name.to_string()))?;

    if repo.get_branch(new_name)?.is_some() {
        return Err(OakError::BranchAlreadyExists(new_name.to_string()));
    }

    let remote_url = repo.get_metadata(MetadataKey::RemoteUrl).ok().flatten();

    let mut remote_branch_missing = false;
    let server_event_id = if let Some(ref remote) = remote_url {
        let (owner, repo_name) = super::read_repo_identity(repo.as_ref())?;
        let api_key = super::credentials::effective_token(
            remote,
            repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
        );

        let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
        match rt.block_on(rename_remote(
            remote,
            &owner,
            &repo_name,
            old_name,
            new_name,
            api_key.as_deref(),
        )) {
            Ok(event) => Some(event.id),
            Err(OakError::BranchNotFound(_)) => {
                remote_branch_missing = true;
                None
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    repo.rename_branch(old_name, new_name)?;

    if let Some(id) = server_event_id {
        repo.set_metadata(MetadataKey::LastRenameId, &id.to_string())?;
    }

    match (remote_url, remote_branch_missing) {
        (Some(_), true) => output::success(&format!(
            "Renamed branch '{old_name}' to '{new_name}' locally (remote branch was not found; run `oak push` to publish)"
        )),
        (Some(_), false) => output::success(&format!(
            "Renamed branch '{old_name}' to '{new_name}' (synced to remote)"
        )),
        (None, _) => output::success(&format!("Renamed branch '{old_name}' to '{new_name}'")),
    }

    Ok(())
}

fn validate_rename_target_name(name: &str) -> Result<()> {
    if name.is_empty() || name.trim() != name {
        return Err(OakError::InvalidArgument(
            "branch name cannot be empty or have leading/trailing whitespace".to_string(),
        ));
    }
    if name == oak_core::DEFAULT_BRANCH {
        return Err(OakError::InvalidArgument(
            "`main` only exists on the server; rename to a personal/feature branch instead"
                .to_string(),
        ));
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
    {
        return Err(OakError::InvalidArgument(
            "branch name cannot contain whitespace, control characters, or path separators"
                .to_string(),
        ));
    }
    Ok(())
}

async fn rename_remote(
    remote: &str,
    owner: &str,
    repo_name: &str,
    old_name: &str,
    new_name: &str,
    api_key: Option<&str>,
) -> Result<RenameBranchResponse> {
    let url = format!(
        "{}/api/{}/{}/branches/{}/rename",
        remote.trim_end_matches('/'),
        owner,
        repo_name,
        super::branch_api_segment(old_name)
    );

    let client = crate::http::api_client();
    let mut req = client
        .post(&url)
        .json(&serde_json::json!({ "new_name": new_name }));
    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(OakError::BranchNotFound(old_name.to_string()));
    }
    if status.as_u16() == 409 {
        return Err(OakError::BranchAlreadyExists(new_name.to_string()));
    }
    if !status.is_success() {
        return Err(OakError::Server(format!(
            "rename failed: {}",
            crate::http::error_text(resp).await
        )));
    }

    resp.json::<RenameBranchResponse>()
        .await
        .map_err(|e| OakError::Http(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{Branch, BranchStatus, CloseReason, Repository, SqliteRepository};
    use tempfile::TempDir;

    #[test]
    fn validate_branch_name_accepts_safe_names() {
        for name in ["mrmrs-a1b2c3", "feature_fix", "release.1"] {
            validate_branch_name(name).unwrap();
        }
    }

    #[test]
    fn validate_branch_name_rejects_names_that_break_remote_paths() {
        for name in [
            "",
            " main",
            "main ",
            "main",
            "feature branch",
            "feature/branch",
            "feature\\branch",
            "feature\nbranch",
            "feature\u{1b}branch",
            "feature\u{7f}branch",
            "feature\0branch",
        ] {
            let err = validate_branch_name(name).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "{name:?} should be InvalidArgument, got {err:?}"
            );
        }
    }

    #[test]
    fn validate_rename_target_name_accepts_safe_names() {
        for name in ["mrmrs-a1b2c3", "feature_fix", "release.1"] {
            validate_rename_target_name(name).unwrap();
        }
    }

    #[test]
    fn validate_rename_target_name_rejects_remote_unsafe_names() {
        for name in [
            "",
            " renamed",
            "renamed ",
            "main",
            "feature branch",
            "feature/branch",
            "feature\\branch",
            "feature\nbranch",
            "feature\u{1b}branch",
            "feature\u{7f}branch",
            "feature\0branch",
        ] {
            let err = validate_rename_target_name(name).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "{name:?} should be InvalidArgument, got {err:?}"
            );
        }
    }

    #[test]
    fn remote_metadata_refresh_replaces_stale_local_row_before_close() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        repo.store_branch(&Branch {
            name: "remote-feature".to_string(),
            description: Some("stale local description".to_string()),
            parent_branch: Some("old-parent".to_string()),
            status: BranchStatus::Open,
            close_reason: Some(CloseReason::parse("stale reason").unwrap()),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        })
        .unwrap();

        let mut branches = vec![
            RemoteBranchData {
                name: "main".to_string(),
                head: None,
                description: Some("fresh main".to_string()),
                parent_branch: None,
                status: "open".to_string(),
                created_at: Some("2026-01-01T00:00:00Z".to_string()),
                close_reason: None,
            },
            RemoteBranchData {
                name: "remote-feature".to_string(),
                head: None,
                description: Some("fresh remote description".to_string()),
                parent_branch: Some("main".to_string()),
                status: "open".to_string(),
                created_at: Some("2026-01-02T00:00:00Z".to_string()),
                close_reason: None,
            },
        ];

        store_remote_branch_metadata(
            &repo,
            &mut branches,
            "remote-feature",
            Some(BranchStatus::Closed),
            Some(CloseReason::parse("superseded remotely").unwrap()),
        )
        .unwrap();

        let refreshed = repo
            .get_branch("remote-feature")
            .unwrap()
            .expect("branch exists");
        assert_eq!(
            refreshed.description.as_deref(),
            Some("fresh remote description")
        );
        assert_eq!(refreshed.parent_branch.as_deref(), Some("main"));
        assert_eq!(refreshed.status, BranchStatus::Closed);
        assert_eq!(
            refreshed.close_reason.as_ref().map(|r| r.as_str()),
            Some("superseded remotely")
        );
        assert_eq!(
            refreshed.created_at.to_rfc3339(),
            "2026-01-02T00:00:00+00:00"
        );
    }
}
