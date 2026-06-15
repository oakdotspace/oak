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

async fn run_inner(path: &Path, description: &str) -> Result<FinishJson> {
    if description.trim().is_empty() {
        return Err(OakError::InvalidArgument(
            "oak finish requires a non-empty --desc or --desc-file".to_string(),
        ));
    }

    let ctx = crate::resolve::resolve(path)?;
    if ctx.oak_dir.join("MERGE_HEAD").exists() {
        return Err(OakError::MergeInProgress);
    }
    if ctx.oak_dir.join("SYNC_HEAD").exists() || ctx.work_tree.join(".oak/SYNC_HEAD").exists() {
        return Err(OakError::MergeFailed(
            "sync is in progress; run `oak pull --continue` after resolving conflicts, \
             or `oak pull --abort`, before `oak finish`"
                .to_string(),
        ));
    }

    let repo = ctx.open()?;
    let branch_name = repo
        .get_current_branch_name()?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;
    if branch.status == BranchStatus::Closed {
        return Err(OakError::BranchClosed(branch_name));
    }

    let head_before = crate::commands::commit::resolve_effective_head(repo.as_ref(), &branch_name)?
        .map(|h| h.to_string());
    let (changes, _, _) = crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;
    let was_dirty = !changes.is_empty();
    let unpushed_before =
        crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?;

    repo.update_branch_description(&branch_name, description)?;

    let mut committed = false;
    if was_dirty {
        crate::commands::commit::run(&ctx.work_tree).map_err(|e| {
            OakError::Server(format!(
                "finish could not commit dirty work: {e}. Run `oak commit`, then `oak push`."
            ))
        })?;
        committed = true;

        let (remaining, _, _) =
            crate::commands::commit::compute_changes(repo.as_ref(), &ctx.work_tree)?;
        if !remaining.is_empty() {
            return Err(OakError::Server(format!(
                "finish could not save all dirty work ({} change(s) remain). \
                 Run `oak status`, then `oak commit`.",
                remaining.len()
            )));
        }
    }

    let unpushed = crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?;
    let mut pushed = false;
    let description_synced;
    if unpushed > 0 {
        let remote = repo
            .get_metadata(MetadataKey::RemoteUrl)?
            .unwrap_or_else(|| "https://oak.space".to_string());
        crate::commands::push::run(&ctx.work_tree, &remote, false, None)
            .await
            .map_err(|e| {
                OakError::Server(format!(
                    "finish could not push {unpushed} unpushed commit(s) on '{branch_name}': {e}. \
                     Run `oak push` to retry."
                ))
            })?;
        pushed = true;
        description_synced = true;
    } else {
        description_synced = sync_description_to_server(repo.as_ref(), &branch_name)
            .await
            .map_err(|e| {
                OakError::Server(format!(
                    "description saved locally but could not sync to the server: {e}. \
                     Run `oak desc --file <file>` to retry."
                ))
            })?;
    }

    let unpushed_after =
        crate::commands::commit::unmerged_commit_count(repo.as_ref(), &branch_name)?;
    if unpushed_after > 0 {
        return Err(OakError::Server(format!(
            "finish pushed but still sees {unpushed_after} unpushed commit(s) on '{branch_name}'. \
             Run `oak push` to retry."
        )));
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

async fn sync_description_to_server(repo: &dyn Repository, branch_name: &str) -> Result<bool> {
    let Some(remote) = repo.get_metadata(MetadataKey::RemoteUrl)? else {
        return Ok(false);
    };
    let Some(owner) = repo.get_metadata(MetadataKey::RepoOwner)? else {
        return Ok(false);
    };
    let Some(repo_name) = repo.get_metadata(MetadataKey::RepoName)? else {
        return Ok(false);
    };
    let remote = remote.trim_end_matches('/').to_string();
    let token = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| crate::commands::credentials::get_token_for_server(&remote));
    crate::commands::push::push_branch_metadata(
        repo,
        &remote,
        &owner,
        &repo_name,
        branch_name,
        token.as_deref(),
    )
    .await?;
    Ok(true)
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
