//! `oak repo visibility` — show or change whether a repository is public.
//!
//! Reads go through `GET /api/{owner}/{name}`; changes go through
//! `PATCH /api/{owner}/{name}/visibility`, which the server only allows for
//! owners and admins of the repo's organization. The repo defaults to the
//! cwd's linked repo; `--repo owner/name` overrides it.

use std::path::Path;

use clap::ValueEnum;
use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use crate::output;

use super::credentials::effective_token_for_worktree;
use super::{parse_owner_repo, read_repo_identity};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Visibility {
    Public,
    Private,
}

impl Visibility {
    fn from_public(is_public: bool) -> Self {
        if is_public {
            Self::Public
        } else {
            Self::Private
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

#[derive(Deserialize)]
struct RepoWire {
    is_public: bool,
}

#[derive(Debug, Serialize)]
pub struct VisibilityJson {
    pub schema_version: u32,
    pub repo: String,
    pub remote: String,
    pub visibility: &'static str,
    pub is_public: bool,
    /// Whether this invocation changed the setting. Absent on a read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    pub recommended_next_commands: Vec<String>,
}

/// Resolve `(owner, name)` from `--repo` or the cwd's linked repo.
fn resolve_repo(work_path: &Path, repo_arg: Option<&str>) -> Result<(String, String)> {
    if let Some(spec) = repo_arg {
        return parse_owner_repo(spec);
    }
    let ctx = crate::resolve::resolve(work_path).map_err(|_| {
        OakError::InvalidArgument(
            "Not inside an Oak repository; pass --repo OWNER/NAME".to_string(),
        )
    })?;
    let repo = ctx.open()?;
    read_repo_identity(&*repo)
}

/// Remote precedence: explicit flag/env (already folded in by the caller),
/// then the checkout's stored remote, then the public default.
fn resolve_remote(work_path: &Path, explicit: Option<&str>) -> String {
    if let Some(remote) = explicit {
        return remote.trim_end_matches('/').to_string();
    }
    crate::resolve::resolve(work_path)
        .ok()
        .and_then(|ctx| ctx.open().ok())
        .and_then(|repo| repo.get_remote_url().ok().flatten())
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| super::push::DEFAULT_REMOTE.to_string())
}

/// Upper bound on a visibility response body; real ones are a few dozen bytes.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Send one request and read `{ "is_public": bool }` back. Like `oak repo
/// list`, this bounds the request time and the body, and never echoes a
/// server error body: error pages can carry server diagnostics.
async fn send(
    req: reqwest::RequestBuilder,
    token: Option<&str>,
    remote: &str,
    full: &str,
    writing: bool,
) -> Result<bool> {
    let timeout = crate::http::read_only_probe_timeout();
    let mut req = req.timeout(timeout);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let remote = crate::http::redact_url_userinfo(remote);
    let transport = |error: reqwest::Error| {
        OakError::Http(format!(
            "repository visibility on {remote}: {}",
            crate::http::transport_failure_reason(&error, timeout)
        ))
    };
    let mut resp = req.send().await.map_err(transport)?;

    let status = resp.status();
    match status.as_u16() {
        200..=299 => {}
        300..=399 => return Err(crate::http::server_error(resp).await),
        401 => {
            return Err(OakError::AuthRequired {
                remote,
                status: 401,
            })
        }
        403 if writing => {
            return Err(OakError::Server(format!(
                "only owners and admins of the organization that owns {full} can change its visibility"
            )));
        }
        403 => {
            return Err(OakError::AuthRequired {
                remote,
                status: 403,
            })
        }
        404 => return Err(OakError::RemoteRepoNotFound(full.to_string())),
        _ => {
            return Err(OakError::Server(format!(
                "repository visibility request failed with HTTP {status}; the response body was omitted because remote diagnostics may contain secrets"
            )));
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(OakError::Server(format!(
                "repository visibility response exceeds the {MAX_RESPONSE_BYTES}-byte client bound"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    let repo: RepoWire = serde_json::from_slice(&body).map_err(|_| {
        OakError::Server("the server returned an unreadable repository record".to_string())
    })?;
    Ok(repo.is_public)
}

/// Show the visibility of a repo, or set it when `target` is given. Setting
/// a repo to what it already is succeeds without a write.
pub async fn run(
    work_path: &Path,
    repo_arg: Option<&str>,
    remote: Option<&str>,
    target: Option<Visibility>,
) -> Result<VisibilityJson> {
    let (owner, name) = resolve_repo(work_path, repo_arg)?;
    let full = format!("{owner}/{name}");
    let remote = resolve_remote(work_path, remote);
    let token = effective_token_for_worktree(&remote, work_path);
    let client = crate::http::api_client();

    let current = send(
        client.get(format!("{remote}/api/{owner}/{name}")),
        token.as_deref(),
        &remote,
        &full,
        false,
    )
    .await?;
    let (is_public, changed) = match target {
        None => (current, None),
        Some(target) if Visibility::from_public(current) == target => (current, Some(false)),
        Some(target) => {
            let req = client
                .patch(format!("{remote}/api/{owner}/{name}/visibility"))
                .json(&serde_json::json!({ "is_public": target == Visibility::Public }));
            let confirmed = send(req, token.as_deref(), &remote, &full, true).await?;
            (confirmed, Some(true))
        }
    };

    let visibility = Visibility::from_public(is_public);
    let flip = match visibility {
        Visibility::Public => Visibility::Private,
        Visibility::Private => Visibility::Public,
    };
    Ok(VisibilityJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: full.clone(),
        remote: crate::http::redact_url_userinfo(&remote),
        visibility: visibility.as_str(),
        is_public,
        changed,
        recommended_next_commands: vec![format!(
            "oak repo visibility {} --repo {full}",
            flip.as_str()
        )],
    })
}

pub fn print_human(result: &VisibilityJson) {
    match result.changed {
        Some(true) => output::success(&format!("{} is now {}", result.repo, result.visibility)),
        Some(false) => output::info(&format!("{} is already {}", result.repo, result.visibility)),
        None => output::print_line(&format!("{} is {}", result.repo, result.visibility)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_repo_wins_over_cwd() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            resolve_repo(temp.path(), Some("zach/vdu")).unwrap(),
            ("zach".to_string(), "vdu".to_string())
        );
    }

    #[test]
    fn outside_a_repo_without_flag_names_the_flag() {
        let temp = tempfile::TempDir::new().unwrap();
        let err = resolve_repo(temp.path(), None).unwrap_err().to_string();
        assert!(err.contains("--repo"), "{err}");
    }

    #[test]
    fn remote_defaults_outside_a_repo_and_strips_trailing_slash() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            resolve_remote(temp.path(), None),
            super::super::push::DEFAULT_REMOTE
        );
        assert_eq!(
            resolve_remote(temp.path(), Some("https://example.test/")),
            "https://example.test"
        );
    }

    #[test]
    fn json_omits_changed_on_read() {
        let json = serde_json::to_value(VisibilityJson {
            schema_version: 1,
            repo: "zach/vdu".into(),
            remote: "https://oak.space".into(),
            visibility: "public",
            is_public: true,
            changed: None,
            recommended_next_commands: vec![],
        })
        .unwrap();
        assert!(json.get("changed").is_none());
        assert_eq!(json["visibility"], "public");
    }
}
