use std::path::Path;

use oak_core::{Branch, BranchStatus, MetadataKey, OakError, Result};
use serde::Deserialize;

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

        super::switch::update_working_dir(root, repo.as_ref(), &manifest)?;
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

/// Close a branch, preventing further commits
pub fn close_branch(path: &Path, name: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    // Verify branch exists
    let _br = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;

    repo.update_branch_status(name, BranchStatus::Closed)?;

    output::success(&format!("Closed branch '{name}'"));

    Ok(())
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
pub fn edit_current_branch(path: &Path, description: &str) -> Result<()> {
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

    Ok(())
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

/// Rename a branch. If a remote is configured, perform the server-side
/// rename first (so other clones discover it on their next pull); only
/// then mirror the rename locally and advance the LastRenameId watermark
/// so we don't replay our own rename. With no remote, this is a pure
/// local rename.
pub fn rename_branch(path: &Path, old_name: &str, new_name: &str) -> Result<()> {
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

    let server_event_id = if let Some(ref remote) = remote_url {
        let (owner, repo_name) = super::read_repo_identity(repo.as_ref())?;
        let api_key = std::env::var("OAK_API_KEY")
            .ok()
            .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
            .or_else(|| super::credentials::get_token_for_server(remote));

        let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
        let event = rt.block_on(rename_remote(
            remote,
            &owner,
            &repo_name,
            old_name,
            new_name,
            api_key.as_deref(),
        ))?;
        Some(event.id)
    } else {
        None
    };

    repo.rename_branch(old_name, new_name)?;

    if let Some(id) = server_event_id {
        repo.set_metadata(MetadataKey::LastRenameId, &id.to_string())?;
    }

    match remote_url {
        Some(_) => output::success(&format!(
            "Renamed branch '{old_name}' to '{new_name}' (synced to remote)"
        )),
        None => output::success(&format!("Renamed branch '{old_name}' to '{new_name}'")),
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
        old_name
    );

    let client = reqwest::Client::new();
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
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "rename failed ({status}): {body}"
        )));
    }

    resp.json::<RenameBranchResponse>()
        .await
        .map_err(|e| OakError::Http(e.to_string()))
}
