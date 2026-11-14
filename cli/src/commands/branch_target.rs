//! The branch an API command points at, and with what credential — shared by
//! `oak approve` and other repo-scoped commands. Two ways to name the target:
//!
//! * from a checkout or mount: the remote, repo, current branch and its head
//!   come from the workspace;
//! * from anywhere (a CI job has only a git snapshot): `--repo OWNER/NAME
//!   --branch NAME`, with `--remote` (default `OAK_REMOTE`, then oak.space).

use std::path::Path;

use oak_core::{OakError, Repository, Result};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Where a branch API command points, and with what credential.
#[derive(Clone)]
pub struct Target {
    pub remote: String,
    pub owner: String,
    pub repo: String,
    pub branch: String,
    /// The branch head in the checkout, when there is one.
    pub local_head: Option<String>,
    token: Option<String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("remote", &self.remote)
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .field("branch", &self.branch)
            .field("local_head", &self.local_head)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The flags every subcommand shares.
#[derive(Debug, Clone, Default)]
pub struct TargetArgs {
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub remote: Option<String>,
}

fn parse_repo_slug(slug: &str) -> Result<(String, String)> {
    let (owner, name) = slug.split_once('/').ok_or_else(|| {
        OakError::InvalidArgument(format!("--repo must be OWNER/NAME, got {slug:?}"))
    })?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(OakError::InvalidArgument(format!(
            "--repo must be OWNER/NAME, got {slug:?}"
        )));
    }
    Ok((owner.to_string(), name.to_string()))
}

fn validate_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 255 || branch.chars().any(char::is_control) {
        return Err(OakError::InvalidArgument(
            "--branch must be a branch name".to_string(),
        ));
    }
    Ok(())
}

impl Target {
    /// Resolve from flags, falling back to the workspace at `path`.
    pub fn resolve(path: &Path, args: &TargetArgs) -> Result<Self> {
        if let Some(slug) = args.repo.as_deref() {
            let (owner, repo) = parse_repo_slug(slug)?;
            let branch = args.branch.clone().ok_or_else(|| {
                OakError::InvalidArgument("--repo needs --branch NAME".to_string())
            })?;
            validate_branch(&branch)?;
            let remote = match args.remote.as_deref() {
                Some(remote) => super::push::normalize_remote_url(remote).ok_or_else(|| {
                    OakError::InvalidArgument("--remote cannot be empty".to_string())
                })?,
                None => super::push::env_remote_override()
                    .unwrap_or_else(|| super::push::DEFAULT_REMOTE.to_string()),
            };
            let token = super::credentials::effective_token(&remote);
            return Ok(Self {
                remote: remote.trim_end_matches('/').to_string(),
                owner,
                repo,
                branch,
                local_head: None,
                token,
            });
        }
        if args.remote.is_some() {
            return Err(OakError::InvalidArgument(
                "--remote applies with --repo; in a checkout the checkout's remote is used"
                    .to_string(),
            ));
        }
        let workspace = super::branch::RemoteWorkspace::open(path)?;
        let current = workspace.repo.get_current_branch_name()?;
        let branch = match args.branch.clone().or(current.clone()) {
            Some(branch) => branch,
            None => {
                return Err(OakError::BranchNotFound(
                    "no current branch; pass --branch NAME".to_string(),
                ))
            }
        };
        validate_branch(&branch)?;
        let local_head = workspace
            .repo
            .get_branch_head(&branch)
            .ok()
            .flatten()
            .map(|h| h.to_string());
        let remote = workspace.remote;
        Ok(Self {
            remote: remote.remote_url,
            owner: remote.owner,
            repo: remote.repo_name,
            branch,
            local_head,
            token: remote.token,
        })
    }

    pub(crate) fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.header("authorization", format!("Bearer {token}")),
            None => req,
        }
    }
}

pub(crate) async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(OakError::Server(
            "server response exceeds the 4 MiB budget".to_string(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
    {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(OakError::Server(
                "server response exceeds the 4 MiB budget".to_string(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_needs_owner_and_name() {
        assert_eq!(
            parse_repo_slug("oak/oak").unwrap(),
            ("oak".to_string(), "oak".to_string())
        );
        for bad in ["oak", "/oak", "oak/", "a/b/c"] {
            assert!(parse_repo_slug(bad).is_err(), "{bad}");
        }
    }
}
