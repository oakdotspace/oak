//! `oak release ...` — manage per-repo releases on the server.
//!
//! Releases are pure server state. There's no local mirror — these
//! commands open the repo only to read its `(owner, name, remote)`
//! triple, then talk to the JSON API at
//! `{remote}/api/{owner}/{name}/releases/...`.
//!
//! Uploading an asset is a two-step content-addressed flow: hash the
//! file locally with BLAKE3, push the bytes to the organization chunk
//! endpoint (same path commits use), then POST a metadata row that
//! points at the hash. The same artifact uploaded twice — across
//! releases or across repos in one organization — dedups in the chunk
//! store.

use std::path::{Path, PathBuf};

use oak_core::{MetadataKey, OakError, Result};
use serde::{Deserialize, Serialize};

use crate::output;

// ---------------------------------------------------------------------------
// Wire types (mirror oak-server/src/api/repo_releases.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CreateReleaseBody<'a> {
    tag_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_hash: Option<&'a str>,
    is_draft: bool,
    is_prerelease: bool,
}

#[derive(Debug, Default, Serialize)]
struct UpdateReleaseBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_prerelease: Option<bool>,
}

#[derive(Debug, Serialize)]
struct AddAssetBody<'a> {
    filename: &'a str,
    content_hash: &'a str,
    size_bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAssetResponse {
    filename: String,
    #[allow(dead_code)]
    content_hash: String,
    size_bytes: i64,
    #[allow(dead_code)]
    content_type: String,
    download_count: i64,
    #[allow(dead_code)]
    created_at: String,
    #[allow(dead_code)]
    download_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    title: String,
    notes: String,
    commit_hash: Option<String>,
    is_draft: bool,
    is_prerelease: bool,
    author: String,
    created_at: String,
    #[allow(dead_code)]
    updated_at: String,
    published_at: Option<String>,
    assets: Vec<ReleaseAssetResponse>,
}

#[derive(Debug, Deserialize)]
struct ReleaseListResponse {
    releases: Vec<ReleaseResponse>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pull `(owner, name, remote)` from the current repo's metadata. Every
/// release command needs all three.
fn repo_remote(cwd: &Path) -> Result<(String, String, String)> {
    let ctx = crate::resolve::resolve(cwd)?;
    let repo = ctx.open()?;
    let (owner, name) = super::read_repo_identity(repo.as_ref())?;
    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server(
            "Repository has no remote URL configured; clone from a server first.".to_string(),
        )
    })?;
    Ok((owner, name, remote))
}

fn auth_token(remote: &str, _cwd: &Path) -> Option<String> {
    std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| super::credentials::get_token_for_server(remote))
}

fn with_auth(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(t) = token {
        builder.header("authorization", format!("Bearer {t}"))
    } else {
        builder
    }
}

async fn read_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    if status.is_redirection() {
        return crate::http::error_text(resp).await;
    }
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiError>(&body) {
        Ok(e) => e.error,
        Err(_) if body.is_empty() => format!("HTTP {status}"),
        Err(_) => body,
    }
}

fn print_release(rel: &ReleaseResponse) {
    let title = if rel.title.is_empty() {
        &rel.tag_name
    } else {
        &rel.title
    };
    let state = if rel.is_draft {
        "draft"
    } else if rel.is_prerelease {
        "pre-release"
    } else {
        "released"
    };
    output::print_line(&format!(
        "{bold}{title}{reset} {dim}({tag} · {state}){reset}",
        bold = output::colors::BOLD,
        reset = output::colors::RESET,
        dim = output::colors::DIM,
        title = title,
        tag = rel.tag_name,
        state = state,
    ));
    output::print_line(&format!(
        "  {dim}by {author} · created {created}{published_part}{reset}",
        dim = output::colors::DIM,
        reset = output::colors::RESET,
        author = rel.author,
        created = rel.created_at,
        published_part = rel
            .published_at
            .as_deref()
            .map(|p| format!(" · published {p}"))
            .unwrap_or_default(),
    ));
    if let Some(ref c) = rel.commit_hash {
        output::print_line(&format!(
            "  {dim}commit {short}{reset}",
            dim = output::colors::DIM,
            reset = output::colors::RESET,
            short = &c[..12.min(c.len())],
        ));
    }
    if !rel.notes.trim().is_empty() {
        output::print_line("");
        for line in rel.notes.lines() {
            output::print_line(&format!("  {line}"));
        }
    }
    if !rel.assets.is_empty() {
        output::print_line("");
        output::print_line(&format!(
            "  {dim}{n} artifact{s}:{reset}",
            dim = output::colors::DIM,
            reset = output::colors::RESET,
            n = rel.assets.len(),
            s = if rel.assets.len() == 1 { "" } else { "s" },
        ));
        for a in &rel.assets {
            output::print_line(&format!(
                "    {fname} {dim}({size} bytes, {n} downloads){reset}",
                dim = output::colors::DIM,
                reset = output::colors::RESET,
                fname = a.filename,
                size = a.size_bytes,
                n = a.download_count,
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn new_release(
    cwd: &Path,
    tag: &str,
    title: Option<&str>,
    notes: Option<&str>,
    commit: Option<&str>,
    draft: bool,
    prerelease: bool,
) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let body = CreateReleaseBody {
        tag_name: tag,
        title: title.filter(|s| !s.is_empty()),
        notes: notes.filter(|s| !s.is_empty()),
        commit_hash: commit.filter(|s| !s.is_empty()),
        is_draft: draft,
        is_prerelease: prerelease,
    };
    let resp = with_auth(
        client.post(format!("{remote}/api/{owner}/{name}/releases")),
        token.as_deref(),
    )
    .json(&body)
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    let rel: ReleaseResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    output::success(&format!(
        "Created release '{}' ({})",
        rel.tag_name,
        if rel.is_draft { "draft" } else { "published" },
    ));
    print_release(&rel);
    Ok(())
}

pub async fn list(cwd: &Path) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let resp = with_auth(
        client.get(format!("{remote}/api/{owner}/{name}/releases")),
        token.as_deref(),
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    let list: ReleaseListResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if list.releases.is_empty() {
        output::info("No releases yet");
        return Ok(());
    }
    for rel in &list.releases {
        print_release(rel);
        output::print_line("");
    }
    Ok(())
}

pub async fn show(cwd: &Path, tag: &str) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let resp = with_auth(
        client.get(format!(
            "{remote}/api/{owner}/{name}/releases/{}",
            urlencoding::encode(tag)
        )),
        token.as_deref(),
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    let rel: ReleaseResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    print_release(&rel);
    Ok(())
}

pub async fn publish(cwd: &Path, tag: &str) -> Result<()> {
    update_release_impl(
        cwd,
        tag,
        UpdateReleaseBody {
            is_draft: Some(false),
            ..Default::default()
        },
        "Published",
    )
    .await
}

pub async fn edit(
    cwd: &Path,
    tag: &str,
    title: Option<&str>,
    notes: Option<&str>,
    draft: Option<bool>,
    prerelease: Option<bool>,
) -> Result<()> {
    update_release_impl(
        cwd,
        tag,
        UpdateReleaseBody {
            title,
            notes,
            is_draft: draft,
            is_prerelease: prerelease,
        },
        "Updated",
    )
    .await
}

async fn update_release_impl(
    cwd: &Path,
    tag: &str,
    body: UpdateReleaseBody<'_>,
    verb: &str,
) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let resp = with_auth(
        client.patch(format!(
            "{remote}/api/{owner}/{name}/releases/{}",
            urlencoding::encode(tag)
        )),
        token.as_deref(),
    )
    .json(&body)
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    let rel: ReleaseResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    output::success(&format!("{verb} release '{}'", rel.tag_name));
    print_release(&rel);
    Ok(())
}

pub async fn delete(cwd: &Path, tag: &str) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let resp = with_auth(
        client.delete(format!(
            "{remote}/api/{owner}/{name}/releases/{}",
            urlencoding::encode(tag)
        )),
        token.as_deref(),
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    output::success(&format!("Deleted release '{tag}'"));
    Ok(())
}

/// Upload one or more files as assets on the given release. Each file
/// is hashed locally, the content is pushed through the organization
/// chunk endpoint (dedupes against existing storage), and a release
/// asset row is created pointing at the BLAKE3 hash.
pub async fn upload(cwd: &Path, tag: &str, files: &[PathBuf]) -> Result<()> {
    if files.is_empty() {
        return Err(OakError::Server(
            "No files specified; pass at least one path to upload.".to_string(),
        ));
    }
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();

    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                OakError::Server(format!(
                    "Could not determine a filename for '{}'",
                    path.display()
                ))
            })?
            .to_string();

        output::info(&format!("Uploading {filename}..."));

        let bytes = std::fs::read(path)
            .map_err(|e| OakError::Server(format!("Failed to read {}: {e}", path.display())))?;
        let size_bytes = bytes.len() as i64;
        let hash = oak_core::hash_bytes(&bytes);
        let content_type = mime_for_filename(&filename);

        // Step 1: push the bytes to the organization chunk store. The
        // server verifies the hash and runs the quota check. We use
        // the existing per-repo chunk endpoint so the organization
        // dedup boundary is consistent with commit blob storage.
        let chunk_resp = with_auth(
            client.put(format!(
                "{remote}/api/{owner}/{name}/chunks/{hash}",
                hash = hash.as_str()
            )),
            token.as_deref(),
        )
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
        if !chunk_resp.status().is_success() {
            return Err(OakError::Server(read_error(chunk_resp).await));
        }

        // Step 2: record the asset metadata. The server re-verifies
        // the hash exists in storage before accepting.
        let meta_body = AddAssetBody {
            filename: &filename,
            content_hash: hash.as_str(),
            size_bytes,
            content_type: Some(content_type),
        };
        let meta_resp = with_auth(
            client.post(format!(
                "{remote}/api/{owner}/{name}/releases/{}/assets",
                urlencoding::encode(tag)
            )),
            token.as_deref(),
        )
        .json(&meta_body)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
        if !meta_resp.status().is_success() {
            return Err(OakError::Server(read_error(meta_resp).await));
        }

        output::success(&format!(
            "Uploaded {filename} ({size_bytes} bytes, blake3 {short}…)",
            short = &hash.as_str()[..12]
        ));
    }
    Ok(())
}

pub async fn delete_asset(cwd: &Path, tag: &str, filename: &str) -> Result<()> {
    let (owner, name, remote) = repo_remote(cwd)?;
    let token = auth_token(&remote, cwd);
    let client = crate::http::api_client();
    let resp = with_auth(
        client.delete(format!(
            "{remote}/api/{owner}/{name}/releases/{}/assets/{}",
            urlencoding::encode(tag),
            urlencoding::encode(filename),
        )),
        token.as_deref(),
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(read_error(resp).await));
    }
    output::success(&format!("Removed asset '{filename}'"));
    Ok(())
}

/// Best-effort content type for an artifact. Extensions we recognize
/// get a real mime; the rest fall through to the octet-stream default
/// the server uses.
fn mime_for_filename(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "7z" => "application/x-7z-compressed",
        "deb" => "application/vnd.debian.binary-package",
        "rpm" => "application/x-rpm",
        "dmg" => "application/x-apple-diskimage",
        "exe" => "application/vnd.microsoft.portable-executable",
        "msi" => "application/x-msi",
        "pkg" => "application/x-newton-compatible-pkg",
        "apk" => "application/vnd.android.package-archive",
        "json" => "application/json",
        "txt" | "md" | "sha256" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
