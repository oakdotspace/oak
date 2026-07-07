//! `oak site` — manage Pages-style static-site hosting from the CLI.
//!
//! Sites are configured per organization. The chosen repo's `main` branch is
//! served at `<organization_slug>.<base_domain>/`. All subcommands hit the
//! server's `/api/orgs/{slug}/site` endpoints (or `/api/sites` for
//! the list view). The organization slug defaults to the owner of the cwd's
//! repo; pass `--organization` (or `--repo owner/name` to `enable`) to
//! override.

use std::path::Path;

use oak_core::{OakError, Result};
use serde::Deserialize;

use crate::output;

use super::credentials::get_token_for_server;
use super::{parse_owner_repo, read_repo_identity};

const DEFAULT_REMOTE: &str = "https://oak.space";

#[derive(Deserialize)]
struct SiteResponse {
    organization_slug: String,
    repo_name: String,
    branch: String,
    source_dir: String,
    /// Full https:// URL like `https://<organization>.oak.space/`, stamped
    /// server-side from `OAK_BASE_DOMAIN`.
    url: String,
    #[allow(dead_code)]
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct SiteListResponse {
    sites: Vec<SiteResponse>,
}

#[derive(Deserialize)]
struct ApiError {
    error: String,
}

/// Resolve the remote URL the CLI should hit. CLI arg wins; otherwise the
/// repo's stored remote; otherwise the public default.
fn resolve_remote(work_path: &Path, override_remote: Option<&str>) -> Result<String> {
    if let Some(remote) = override_remote {
        let Some(remote) = super::push::normalize_remote_url(remote) else {
            return Err(OakError::InvalidArgument(
                "remote URL cannot be empty".to_string(),
            ));
        };
        return Ok(remote);
    }
    if let Some(r) = super::push::env_remote_override() {
        return Ok(r);
    }
    if let Ok(ctx) = crate::resolve::resolve(work_path) {
        if let Ok(repo) = ctx.open() {
            if let Ok(Some(url)) = repo.get_remote_url() {
                if !url.trim().is_empty() {
                    return Ok(url.trim_end_matches('/').to_string());
                }
            }
        }
    }
    Ok(DEFAULT_REMOTE.to_string())
}

/// Resolve `(owner, name)` from either an explicit `--repo` flag or the
/// cwd's repo metadata. Used by `enable`, where we need both the
/// organization (owner) and the repo to publish.
fn resolve_repo(work_path: &Path, repo_arg: Option<&str>) -> Result<(String, String)> {
    if let Some(spec) = repo_arg {
        return parse_owner_repo(spec);
    }
    let ctx = crate::resolve::resolve(work_path)?;
    let repo = ctx.open()?;
    read_repo_identity(&*repo)
}

/// Resolve just the organization slug: either the explicit `--organization`
/// flag or the cwd's repo owner. Used by `show` and `disable`, which
/// don't care which repo is currently being served.
fn resolve_organization(work_path: &Path, organization_arg: Option<&str>) -> Result<String> {
    if let Some(slug) = organization_arg {
        let slug = slug.trim();
        validate_organization_slug(slug)?;
        return Ok(slug.to_string());
    }
    let ctx = crate::resolve::resolve(work_path)?;
    let repo = ctx.open()?;
    let (owner, _) = read_repo_identity(&*repo)?;
    validate_organization_slug(&owner)?;
    Ok(owner)
}

fn validate_organization_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug == "." || slug == ".." {
        return Err(OakError::InvalidArgument(
            "organization slug must be a non-empty path segment".to_string(),
        ));
    }
    if slug
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
    {
        return Err(OakError::InvalidArgument(
            "organization slug cannot contain whitespace, control characters, or path separators"
                .to_string(),
        ));
    }
    Ok(())
}

/// Parse the server's response, treating non-success status codes as
/// errors with the server-supplied message.
async fn parse_response<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<T>()
            .await
            .map_err(|e| OakError::Http(e.to_string()));
    }
    if status.is_redirection() {
        return Err(crate::http::server_error(resp).await);
    }
    let text = resp.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<ApiError>(&text)
        .map(|e| e.error)
        .unwrap_or(text);
    if msg.is_empty() {
        return Err(OakError::Server(format!("HTTP {status}")));
    }
    Err(OakError::Server(format!("{status}: {msg}")))
}

/// `oak site enable` — pick a repo for the organization's static site.
pub async fn enable(
    work_path: &Path,
    repo_arg: Option<&str>,
    remote_arg: Option<&str>,
    branch: Option<&str>,
    source_dir: Option<&str>,
) -> Result<()> {
    let (owner, name) = resolve_repo(work_path, repo_arg)?;
    let remote = resolve_remote(work_path, remote_arg)?;
    let token = get_token_for_server(&remote);

    let mut body = serde_json::json!({ "repo": name });
    if let Some(b) = branch {
        body["branch"] = serde_json::json!(b);
    }
    if let Some(d) = source_dir {
        body["source_dir"] = serde_json::json!(d);
    }

    let client = crate::http::api_client();
    let mut req = client
        .put(format!("{remote}/api/orgs/{owner}/site"))
        .json(&body);
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let site: SiteResponse = parse_response(resp).await?;

    output::success(&format!(
        "Site enabled for organization '{}' — serving '{}' at {}",
        site.organization_slug, site.repo_name, site.url
    ));
    println!("  branch:     {}", site.branch);
    println!("  source dir: {}", site.source_dir);
    Ok(())
}

/// `oak site disable` — turn off the organization's site.
pub async fn disable(
    work_path: &Path,
    organization_arg: Option<&str>,
    remote_arg: Option<&str>,
) -> Result<()> {
    let slug = resolve_organization(work_path, organization_arg)?;
    let remote = resolve_remote(work_path, remote_arg)?;
    let token = get_token_for_server(&remote);

    let client = crate::http::api_client();
    let mut req = client.delete(format!("{remote}/api/orgs/{slug}/site"));
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    output::success(&format!("Site disabled for organization '{slug}'"));
    Ok(())
}

/// `oak site show` — print the site config for an organization.
pub async fn show(
    work_path: &Path,
    organization_arg: Option<&str>,
    remote_arg: Option<&str>,
) -> Result<()> {
    let slug = resolve_organization(work_path, organization_arg)?;
    let remote = resolve_remote(work_path, remote_arg)?;

    let client = crate::http::api_client();
    let resp = client
        .get(format!("{remote}/api/orgs/{slug}/site"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().as_u16() == 404 {
        output::info(&format!(
            "No site enabled for organization '{slug}'. Run `oak site enable` to publish one."
        ));
        return Ok(());
    }
    let site: SiteResponse = parse_response(resp).await?;
    println!("Site for organization {}:", site.organization_slug);
    println!("  url:        {}", site.url);
    println!("  repo:       {}", site.repo_name);
    println!("  branch:     {}", site.branch);
    println!("  source dir: {}", site.source_dir);
    println!("  updated:    {}", site.updated_at);
    Ok(())
}

/// `oak site list` — list all organization sites the caller can see.
pub async fn list(work_path: &Path, remote_arg: Option<&str>) -> Result<()> {
    let remote = resolve_remote(work_path, remote_arg)?;
    let token = get_token_for_server(&remote);
    if token.is_none() {
        return Err(OakError::Server(
            "Not logged in. Run `oak login` first.".to_string(),
        ));
    }

    let client = crate::http::api_client();
    let mut req = client.get(format!("{remote}/api/sites"));
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let list: SiteListResponse = parse_response(resp).await?;

    if list.sites.is_empty() {
        output::info("No sites yet. Run `oak site enable` from a repo to publish one.");
        return Ok(());
    }
    output::info(&format!("Sites ({}):", list.sites.len()));
    println!();
    for s in list.sites {
        println!("  {}  →  {}", s.organization_slug, s.url);
        println!(
            "    repo: {}, branch: {}, source: {}",
            s.repo_name, s.branch, s.source_dir
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_organization_slug_accepts_safe_slugs() {
        for slug in ["oak", "team-tools", "docs_v2"] {
            validate_organization_slug(slug).unwrap();
        }
    }

    #[test]
    fn validate_organization_slug_rejects_url_path_breakers() {
        for slug in [
            "",
            ".",
            "..",
            "team docs",
            "team/docs",
            "team\\docs",
            "team\ndocs",
            "team\u{1b}docs",
            "team\u{7f}docs",
            "team\0docs",
        ] {
            let err = validate_organization_slug(slug).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "{slug:?} should be InvalidArgument, got {err:?}"
            );
        }
    }

    #[test]
    fn resolve_remote_rejects_explicit_blank_before_fallback() {
        let temp = tempfile::TempDir::new().unwrap();
        let err = resolve_remote(temp.path(), Some(" \t ")).unwrap_err();
        assert!(
            matches!(err, OakError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );
    }
}
