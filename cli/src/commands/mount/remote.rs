//! HTTP helpers used by the mount feature.
//!
//! Most blob/chunk fetching is delegated to [`crate::commands::blob_fetch`]
//! which already implements the lazy-fetch flow we want. This module adds
//! the bits the mount needs on top of that:
//!
//! 1. resolving a branch on the remote to its head commit hash
//! 2. fetching just the head commit + manifest into the mount's cache db,
//!    without pulling any blobs

use oak_core::SqliteRepository;
use oak_core::{Hash, OakError, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct BranchInfo {
    pub name: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub parent_branch: Option<String>,
}

#[derive(Deserialize)]
struct RepoInfo {
    #[serde(default)]
    head: Option<String>,
}

#[derive(Deserialize)]
struct BranchListResp {
    branches: Vec<BranchInfo>,
}

fn auth(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(t) = token {
        builder.header("authorization", format!("Bearer {t}"))
    } else {
        builder
    }
}

/// Resolve `branch` on the remote to its head commit hash. If `branch` is
/// `None`, returns the repo's overall head and the branch it lives on
/// (preferring `main`, then `master`).
pub async fn resolve_branch_head(
    remote_url: &str,
    owner: &str,
    repo: &str,
    branch: Option<&str>,
    token: Option<&str>,
) -> Result<(String, String)> {
    let client = reqwest::Client::new();

    if let Some(branch_name) = branch {
        let url = format!("{remote_url}/api/{owner}/{repo}/branches/{branch_name}");
        let resp = auth(client.get(&url), token)
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        if resp.status().as_u16() == 404 {
            return Err(OakError::Server(format!(
                "branch '{branch_name}' not found on {owner}/{repo}"
            )));
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OakError::Server(body));
        }
        let info: BranchInfo = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        let head = info.head.ok_or_else(|| {
            OakError::Server(format!(
                "branch '{branch_name}' has no commits — nothing to mount"
            ))
        })?;
        return Ok((info.name, head));
    }

    let repo_url = format!("{remote_url}/api/{owner}/{repo}");
    let resp = auth(client.get(&repo_url), token)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(body));
    }
    let info: RepoInfo = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let head = info
        .head
        .ok_or_else(|| OakError::Server("repository has no commits — nothing to mount".into()))?;

    let list_url = format!("{remote_url}/api/{owner}/{repo}/branches");
    let resp = auth(client.get(&list_url), token)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let list: BranchListResp = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let mut candidates: Vec<&BranchInfo> = list
        .branches
        .iter()
        .filter(|b| b.head.as_deref() == Some(&head))
        .collect();
    candidates.sort_by_key(|b| match b.name.as_str() {
        "main" => 0,
        "master" => 1,
        _ => 2,
    });
    let chosen = candidates
        .first()
        .map(|b| b.name.clone())
        .or_else(|| list.branches.first().map(|b| b.name.clone()))
        .unwrap_or_else(|| "main".to_string());

    Ok((chosen, head))
}

/// Fetch a commit + its manifest into the cache, without fetching any blobs.
pub async fn fetch_commit_into_cache(
    cache: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo: &str,
    head: &Hash,
    token: Option<&str>,
) -> Result<()> {
    super::super::blob_fetch::ensure_commits_local(
        cache,
        remote_url,
        owner,
        repo,
        token,
        std::slice::from_ref(head),
    )
    .await
}

/// Resolve a team or project slug to the list of `path_prefix` values
/// that bound it for this repo. Used by `oak mount --team/--project` to
/// learn the prefix set before the FUSE / ProjFS tree is built.
///
/// Returns an empty list when neither `team` nor `project` is set
/// (whole-repo mount).
pub async fn resolve_scope_prefixes(
    remote_url: &str,
    owner: &str,
    repo: &str,
    team: Option<&str>,
    project: Option<&str>,
    token: Option<&str>,
) -> Result<Vec<String>> {
    if team.is_none() && project.is_none() {
        return Ok(Vec::new());
    }

    let client = reqwest::Client::new();
    let mut params: Vec<String> = Vec::new();
    if let Some(t) = team {
        params.push(format!("team={t}"));
    }
    if let Some(p) = project {
        params.push(format!("project={p}"));
    }
    let url = format!("{remote_url}/api/{owner}/{repo}/scope?{}", params.join("&"));
    let resp = auth(client.get(&url), token)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if resp.status().as_u16() == 404 {
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(body));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "scope resolution failed ({status}): {body}"
        )));
    }

    #[derive(Deserialize)]
    struct ScopeResp {
        path_prefixes: Vec<String>,
    }

    let body: ScopeResp = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    Ok(body.path_prefixes)
}

/// Pre-fetch the *sizes* of every blob referenced by a manifest. The blob
/// sizes are returned by `/blobs/info` along with chunk metadata; we just
/// pull the size out so the FUSE layer can serve `getattr` without fetching
/// content.
pub async fn fetch_blob_sizes(
    remote_url: &str,
    owner: &str,
    repo: &str,
    blob_hashes: &[String],
    token: Option<&str>,
) -> Result<Vec<(String, u64)>> {
    if blob_hashes.is_empty() {
        return Ok(Vec::new());
    }
    let client = reqwest::Client::new();
    let url = format!("{remote_url}/api/{owner}/{repo}/blobs/info");

    // Batch into chunks so we don't hit body limits.
    const BATCH: usize = 1000;
    let mut out = Vec::with_capacity(blob_hashes.len());
    for batch in blob_hashes.chunks(BATCH) {
        let req_body = serde_json::json!({ "hashes": batch });
        let resp = auth(client.post(&url).json(&req_body), token)
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OakError::Server(format!(
                "blob info request failed ({status}): {body}"
            )));
        }

        #[derive(Deserialize)]
        struct BlobInfoBlob {
            hash: String,
            size: u64,
        }
        #[derive(Deserialize)]
        struct BlobInfoResponse {
            blobs: Vec<BlobInfoBlob>,
        }

        let body: BlobInfoResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        for b in body.blobs {
            out.push((b.hash, b.size));
        }
    }
    Ok(out)
}
