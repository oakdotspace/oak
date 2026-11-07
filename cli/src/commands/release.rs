//! `oak release` (alias `oak tag`): repository tags, which are also releases.
//!
//! A tag is an immutable name on a commit on `main`, and every tag is a
//! release: title, notes, draft/prerelease and assets are all optional. The
//! server owns tags (`/api/{owner}/{name}/
//! tags`, `release_protocol` v1); `oak pull` and `oak clone` mirror every
//! published tag and its asset list into the local cache, but never asset
//! bytes. Assets are fetched only by `oak release download`.
//!
//! Asset bytes use the same content-defined chunking and chunk store as file
//! content: `upload` chunks the file exactly as push chunks a large file,
//! uploads only the chunks the server lacks (an asset that shares bytes with
//! the repo or an earlier release reuses them), then registers the blob on
//! the tag. The server proves the chunks hash to the blob before listing it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use oak_core::protocol::TagData;
use oak_core::{LocalRelease, OakError, Result, SqliteRepository};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::output;

use super::credentials::effective_token_for_worktree;
use super::{parse_owner_repo, read_repo_identity};

/// Mirror the server's tag snapshot from a pull or clone into the local
/// cache. `None` (an older server, or releases disabled for the repo) leaves
/// the cache untouched; `Some` replaces every synced tag, so deletions
/// propagate.
pub(crate) fn sync_tag_snapshot(repo: &SqliteRepository, tags: Option<&[TagData]>) -> Result<()> {
    let Some(tags) = tags else {
        return Ok(());
    };
    let snapshot: Vec<LocalRelease> = tags.iter().map(LocalRelease::from).collect();
    repo.replace_synced_releases(&snapshot)
}

/// Chunks uploaded per `/chunks/check` page. FastCDC caps a chunk at 4 MiB,
/// so a page holds at most 32 MiB in memory.
const UPLOAD_PAGE_CHUNKS: usize = 8;
/// Bound on a JSON response from the tags API (a tag with 1000 assets and
/// 128 KiB of notes stays well under this).
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Registering a large asset makes the server read every chunk back.
const REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

// ---------------------------------------------------------------- wire types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetWire {
    pub name: String,
    pub blob_hash: String,
    pub size: u64,
    pub content_type: String,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub download_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagWire {
    pub name: String,
    pub commit: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub finalized: bool,
    #[serde(default)]
    pub created_by: String,
    pub created_at: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub revision: i64,
    #[serde(default)]
    pub assets: Vec<AssetWire>,
}

impl TagWire {
    fn to_local(&self) -> LocalRelease {
        LocalRelease {
            name: self.name.clone(),
            commit_hash: self.commit.clone(),
            title: self.title.clone(),
            notes: self.notes.clone(),
            prerelease: self.prerelease,
            finalized: self.finalized,
            created_by: self.created_by.clone(),
            created_at: self.created_at.clone(),
            published_at: self.published_at.clone(),
            revision: self.revision,
            synced: true,
            assets: self
                .assets
                .iter()
                .map(|a| oak_core::LocalReleaseAsset {
                    name: a.name.clone(),
                    blob_hash: a.blob_hash.clone(),
                    size: a.size,
                    content_type: a.content_type.clone(),
                })
                .collect(),
        }
    }

    fn from_local(local: &LocalRelease) -> Self {
        Self {
            name: local.name.clone(),
            commit: local.commit_hash.clone(),
            title: local.title.clone(),
            notes: local.notes.clone(),
            draft: local.synced && local.published_at.is_none(),
            prerelease: local.prerelease,
            finalized: local.finalized,
            created_by: local.created_by.clone(),
            created_at: local.created_at.clone(),
            published_at: local.published_at.clone(),
            revision: local.revision,
            assets: local
                .assets
                .iter()
                .map(|a| AssetWire {
                    name: a.name.clone(),
                    blob_hash: a.blob_hash.clone(),
                    size: a.size,
                    content_type: a.content_type.clone(),
                    download_count: 0,
                    download_path: None,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct TagEnvelope {
    tag: TagWire,
    #[serde(default)]
    created: Option<bool>,
}

#[derive(Deserialize)]
struct ListEnvelope {
    tags: Vec<TagWire>,
    #[serde(default)]
    next_before: Option<i64>,
}

#[derive(Deserialize)]
struct DeleteEnvelope {
    deleted: TagWire,
}

#[derive(Deserialize)]
struct AssetEnvelope {
    #[serde(default)]
    added: bool,
    asset: AssetWire,
}

#[derive(Deserialize)]
struct ApiErrorWire {
    error: String,
    #[serde(default)]
    code: Option<String>,
}

// ---------------------------------------------------------------- target

/// The repo a command acts on, and the local cache when the command runs
/// inside a checkout of that same repo.
pub struct Target {
    owner: String,
    name: String,
    full: String,
    remote: String,
    token: Option<String>,
    local: Option<SqliteRepository>,
    /// False for a checkout not yet linked to a remote (fresh `oak clone
    /// <git-url>`): its local tags can be read, nothing else.
    linked: bool,
}

impl Target {
    pub fn resolve(
        work_path: &Path,
        repo_arg: Option<&str>,
        remote_arg: Option<&str>,
    ) -> Result<Self> {
        let checkout = crate::resolve::resolve(work_path).ok().and_then(|ctx| {
            let db = ctx.db_path().ok()?;
            let repo = SqliteRepository::open(&db).ok()?;
            let identity = read_repo_identity(&repo).ok();
            let remote = oak_core::Repository::get_remote_url(&repo)
                .ok()
                .flatten()
                .map(|url| url.trim().trim_end_matches('/').to_string())
                .filter(|url| !url.is_empty());
            Some((repo, identity, remote))
        });
        let (owner, name, linked) = match (repo_arg, &checkout) {
            (Some(spec), _) => {
                let (owner, name) = parse_owner_repo(spec)?;
                (owner, name, true)
            }
            (None, Some((_, Some((owner, name)), _))) => (owner.clone(), name.clone(), true),
            (None, Some((_, None, _))) => (String::new(), String::new(), false),
            (None, None) => {
                return Err(OakError::InvalidArgument(
                    "Not inside an Oak repository; pass --repo OWNER/NAME".to_string(),
                ))
            }
        };
        let (local, checkout_remote) = match checkout {
            Some((repo, None, remote)) if !linked => (Some(repo), remote),
            Some((repo, Some(identity), remote)) if identity == (owner.clone(), name.clone()) => {
                (Some(repo), remote)
            }
            _ => (None, None),
        };
        let remote = remote_arg
            .map(|r| r.trim_end_matches('/').to_string())
            .or(checkout_remote)
            .unwrap_or_else(|| super::push::DEFAULT_REMOTE.to_string());
        let token = effective_token_for_worktree(&remote, work_path);
        Ok(Self {
            full: if linked {
                format!("{owner}/{name}")
            } else {
                "this checkout".to_string()
            },
            linked,
            owner,
            name,
            remote,
            token,
            local,
        })
    }

    fn url(&self, rest: &str) -> String {
        format!("{}/api/{}/{}/{rest}", self.remote, self.owner, self.name)
    }

    fn tag_url(&self, tag: &str, rest: &str) -> String {
        let tag = urlencoding::encode(tag);
        if rest.is_empty() {
            self.url(&format!("tags/{tag}"))
        } else {
            self.url(&format!("tags/{tag}/{rest}"))
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.token.as_deref() {
            Some(t) => req.header("authorization", format!("Bearer {t}")),
            None => req,
        }
    }

    /// Send a tags-API request and decode its JSON body. Error bodies from
    /// this API are Oak's own `{error, code}` records, so their message is
    /// shown; anything else is reported by status alone.
    fn require_linked(&self) -> Result<()> {
        if self.linked {
            Ok(())
        } else {
            Err(OakError::Config(
                "This checkout isn't linked to an Oak remote yet; publish it with `oak push --repo ORG/REPO` first (or pass --offline to read local tags)".to_string(),
            ))
        }
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        timeout: std::time::Duration,
    ) -> Result<T> {
        self.require_linked()?;
        let remote = crate::http::redact_url_userinfo(&self.remote);
        let transport = |error: reqwest::Error| {
            OakError::Http(format!(
                "releases on {remote}: {}",
                crate::http::transport_failure_reason(&error, timeout)
            ))
        };
        let mut resp = self
            .auth(req.timeout(timeout))
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(transport)? {
            if chunk.len() > MAX_JSON_BYTES.saturating_sub(body.len()) {
                return Err(OakError::Server(format!(
                    "releases response exceeds the {MAX_JSON_BYTES}-byte client bound"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|_| {
                OakError::Server("the server returned an unreadable releases record".to_string())
            });
        }
        let api_error: Option<ApiErrorWire> = serde_json::from_slice(&body).ok();
        Err(self.status_error(status.as_u16(), api_error))
    }

    fn status_error(&self, status: u16, api_error: Option<ApiErrorWire>) -> OakError {
        let remote = crate::http::redact_url_userinfo(&self.remote);
        let message = api_error.as_ref().map(|e| e.error.clone());
        let code = api_error.and_then(|e| e.code);
        match status {
            401 => OakError::AuthRequired {
                remote,
                status: 401,
            },
            403 => match message {
                Some(m) => OakError::Server(m),
                None => OakError::AuthRequired {
                    remote,
                    status: 403,
                },
            },
            // The tags routes answer a bare 404 when releases are off for
            // this repo (or the repo is invisible); a named tag/asset miss
            // carries its own message.
            404 => match (code.as_deref(), message) {
                (Some("not_found"), Some(m)) if m != "Not found" => OakError::InvalidArgument(m),
                _ => OakError::Server(format!(
                    "releases are not available for {} on {remote} (the repository is missing, \
                     not visible to you, or does not have releases enabled)",
                    self.full
                )),
            },
            400 | 422 => OakError::InvalidArgument(
                message
                    .unwrap_or_else(|| format!("the server rejected the request (HTTP {status})")),
            ),
            _ => OakError::Server(match (message, code) {
                (Some(m), Some(c)) => format!("{m} ({c})"),
                (Some(m), None) => m,
                _ => format!(
                    "releases request failed with HTTP {status}; the response body was omitted"
                ),
            }),
        }
    }

    fn cache_upsert(&self, tag: &TagWire) {
        if let Some(local) = &self.local {
            if let Err(e) = local.upsert_synced_release(&tag.to_local()) {
                output::warning(&format!("could not update the local tag cache: {e}"));
            }
        }
    }

    fn cache_remove(&self, name: &str) {
        if let Some(local) = &self.local {
            if let Err(e) = local.remove_release(name) {
                output::warning(&format!("could not update the local tag cache: {e}"));
            }
        }
    }

    async fn fetch_tag(&self, name: &str) -> Result<TagWire> {
        let client = crate::http::api_client();
        let envelope: TagEnvelope = self
            .send(
                client.get(self.tag_url(name, "")),
                crate::http::read_only_probe_timeout(),
            )
            .await?;
        Ok(envelope.tag)
    }
}

fn repo_flag(target: &Target) -> String {
    if target.linked {
        format!(" --repo {}", target.full)
    } else {
        String::new()
    }
}

fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+/-:=".contains(&b))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------- list / view

#[derive(Debug, Serialize)]
pub struct ListJson {
    pub schema_version: u32,
    pub repo: String,
    /// `remote` when read from the server, `local` when read from the
    /// checkout's cache (`--offline`, last refreshed by `oak pull`).
    pub source: &'static str,
    pub tags: Vec<TagWire>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub recommended_next_commands: Vec<String>,
}

pub async fn list(target: &Target, limit: usize, offline: bool) -> Result<ListJson> {
    let (source, mut tags, truncated) = if offline {
        let local = target.local.as_ref().ok_or_else(|| {
            OakError::InvalidArgument(
                "--offline reads this checkout's tag cache; run it inside a checkout of the repo"
                    .to_string(),
            )
        })?;
        let all: Vec<TagWire> = local
            .list_releases()?
            .iter()
            .map(TagWire::from_local)
            .collect();
        ("local", all, false)
    } else {
        let client = crate::http::api_client();
        let mut out = Vec::new();
        let mut before: Option<i64> = None;
        let mut truncated = false;
        loop {
            let mut url = format!("{}?limit=200", target.url("tags"));
            if let Some(b) = before {
                url.push_str(&format!("&before={b}"));
            }
            let page: ListEnvelope = target
                .send(client.get(url), crate::http::read_only_probe_timeout())
                .await?;
            out.extend(page.tags);
            if out.len() >= limit {
                truncated = page.next_before.is_some() || out.len() > limit;
                break;
            }
            match page.next_before {
                Some(b) => before = Some(b),
                None => break,
            }
        }
        // A full remote listing refreshes the cache; drafts are not cached
        // because pull never syncs them.
        if !truncated {
            if let Some(local) = &target.local {
                let snapshot: Vec<LocalRelease> = out
                    .iter()
                    .filter(|t| !t.draft)
                    .map(TagWire::to_local)
                    .collect();
                if let Err(e) = local.replace_synced_releases(&snapshot) {
                    output::warning(&format!("could not update the local tag cache: {e}"));
                }
            }
        }
        ("remote", out, truncated)
    };
    let truncated = truncated || tags.len() > limit;
    tags.truncate(limit);
    let mut next = Vec::new();
    if let Some(first) = tags.first() {
        next.push(format!(
            "oak release view {}{}",
            quote(&first.name),
            repo_flag(target)
        ));
    } else {
        next.push(format!("oak release create v0.1.0{}", repo_flag(target)));
    }
    Ok(ListJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        source,
        tags,
        truncated,
        recommended_next_commands: next,
    })
}

pub fn print_list(result: &ListJson) {
    if result.tags.is_empty() {
        output::info(&format!("No tags in {}", result.repo));
        return;
    }
    for tag in &result.tags {
        let mut badges = Vec::new();
        if tag.draft {
            badges.push("draft".to_string());
        }
        if tag.prerelease {
            badges.push("prerelease".to_string());
        }
        if !tag.assets.is_empty() {
            badges.push(format!(
                "{} asset{}",
                tag.assets.len(),
                if tag.assets.len() == 1 { "" } else { "s" }
            ));
        }
        let title = tag
            .title
            .as_deref()
            .map(|t| format!("  {t}"))
            .unwrap_or_default();
        let badges = if badges.is_empty() {
            String::new()
        } else {
            format!("  [{}]", badges.join(", "))
        };
        output::print_line(&format!(
            "{:<24} {}{title}{badges}",
            tag.name,
            &tag.commit[..tag.commit.len().min(12)]
        ));
    }
    if result.truncated {
        output::info("(more tags exist; raise --limit)");
    }
}

#[derive(Debug, Serialize)]
pub struct TagJson {
    pub schema_version: u32,
    pub repo: String,
    pub source: &'static str,
    pub tag: TagWire,
    /// Assets already saved under the download directory are not tracked;
    /// this lists what the release offers.
    pub recommended_next_commands: Vec<String>,
}

fn tag_next_commands(target: &Target, tag: &TagWire) -> Vec<String> {
    let name = quote(&tag.name);
    let repo = repo_flag(target);
    let mut next = Vec::new();
    if tag.draft {
        next.push(format!("oak release edit {name} --publish{repo}"));
    }
    if !tag.assets.is_empty() {
        next.push(format!("oak release download {name}{repo}"));
    }
    if !tag.finalized {
        next.push(format!("oak release upload {name} FILE...{repo}"));
    }
    next
}

pub async fn view(target: &Target, name: &str, offline: bool) -> Result<TagJson> {
    let (source, tag) = if offline {
        let local = target.local.as_ref().ok_or_else(|| {
            OakError::InvalidArgument(
                "--offline reads this checkout's tag cache; run it inside a checkout of the repo"
                    .to_string(),
            )
        })?;
        let found = local.get_release(name)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "no tag named '{name}' in the local cache; run `oak pull` or drop --offline"
            ))
        })?;
        ("local", TagWire::from_local(&found))
    } else {
        let tag = target.fetch_tag(name).await?;
        if !tag.draft {
            target.cache_upsert(&tag);
        }
        ("remote", tag)
    };
    Ok(TagJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        source,
        recommended_next_commands: tag_next_commands(target, &tag),
        tag,
    })
}

pub fn print_tag(tag: &TagWire) {
    let mut header = format!("Release {}", tag.name);
    if tag.draft {
        header.push_str(" (draft)");
    }
    if tag.prerelease {
        header.push_str(" (prerelease)");
    }
    output::print_line(&header);
    if let Some(title) = &tag.title {
        output::print_line(&format!("  title:     {title}"));
    }
    output::print_line(&format!("  commit:    {}", tag.commit));
    if !tag.created_by.is_empty() {
        output::print_line(&format!("  by:        {}", tag.created_by));
    }
    output::print_line(&format!(
        "  published: {}",
        tag.published_at.as_deref().unwrap_or("not yet (draft)")
    ));
    if tag.finalized {
        output::print_line("  assets are final");
    }
    if let Some(notes) = tag.notes.as_deref().filter(|n| !n.is_empty()) {
        output::print_line("");
        for line in notes.lines() {
            output::print_line(&format!("  {line}"));
        }
    }
    if !tag.assets.is_empty() {
        output::print_line("");
        output::print_line("  assets:");
        for asset in &tag.assets {
            output::print_line(&format!(
                "    {:<40} {:>12}  {}",
                asset.name,
                human_bytes(asset.size),
                &asset.blob_hash[..asset.blob_hash.len().min(12)]
            ));
        }
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------- create / edit / delete

pub struct CreateOptions {
    pub name: String,
    pub commit: Option<String>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub finalize: bool,
    pub assets: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct MutationJson {
    pub schema_version: u32,
    pub repo: String,
    /// `created`, `unchanged`, `updated`, `deleted`, `uploaded`, `removed`.
    pub outcome: &'static str,
    pub tag: TagWire,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<UploadJson>,
    pub recommended_next_commands: Vec<String>,
}

/// Resolve `--commit`: a full hash passes through; a unique prefix is
/// expanded from local history (the server only accepts full hashes).
fn resolve_commit(target: &Target, commit: &str) -> Result<String> {
    let commit = commit.trim().to_ascii_lowercase();
    if commit.len() == 64 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(commit);
    }
    let local = target.local.as_ref().ok_or_else(|| {
        OakError::InvalidArgument(
            "--commit needs a full 64-character hash outside a checkout".to_string(),
        )
    })?;
    Ok(super::checkout::resolve_commit_ref(local, &commit)?.to_string())
}

pub async fn create(target: &Target, opts: CreateOptions) -> Result<MutationJson> {
    for path in &opts.assets {
        check_asset_file(path)?;
    }
    let commit = opts
        .commit
        .as_deref()
        .map(|c| resolve_commit(target, c))
        .transpose()?;
    let client = crate::http::api_client();
    let mut body = json!({
        "name": opts.name,
        "draft": opts.draft,
        "prerelease": opts.prerelease,
    });
    if let Some(c) = commit {
        body["commit"] = json!(c);
    }
    if let Some(t) = &opts.title {
        body["title"] = json!(t);
    }
    if let Some(n) = &opts.notes {
        body["notes"] = json!(n);
    }
    let envelope: TagEnvelope = target
        .send(
            client.post(target.url("tags")).json(&body),
            crate::http::read_only_probe_timeout(),
        )
        .await?;
    let outcome = if envelope.created == Some(false) {
        "unchanged"
    } else {
        "created"
    };
    let mut tag = envelope.tag;
    let mut uploads = Vec::new();
    for path in &opts.assets {
        uploads.push(upload_one(target, &tag.name, path, None).await?);
    }
    if opts.finalize {
        tag = patch(target, &tag.name, json!({ "finalize": true })).await?;
    } else if !uploads.is_empty() {
        tag = target.fetch_tag(&tag.name).await?;
    }
    if !tag.draft {
        target.cache_upsert(&tag);
    }
    Ok(MutationJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        outcome,
        recommended_next_commands: tag_next_commands(target, &tag),
        tag,
        uploads,
    })
}

async fn patch(target: &Target, name: &str, body: Value) -> Result<TagWire> {
    let client = crate::http::api_client();
    let envelope: TagEnvelope = target
        .send(
            client.patch(target.tag_url(name, "")).json(&body),
            crate::http::read_only_probe_timeout(),
        )
        .await?;
    Ok(envelope.tag)
}

#[derive(Default)]
pub struct EditOptions {
    pub title: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub prerelease: Option<bool>,
    pub publish: bool,
    pub finalize: bool,
    pub expected_revision: Option<i64>,
}

pub async fn edit(target: &Target, name: &str, opts: EditOptions) -> Result<MutationJson> {
    let mut body = serde_json::Map::new();
    if let Some(title) = opts.title {
        body.insert("title".into(), json!(title));
    }
    if let Some(notes) = opts.notes {
        body.insert("notes".into(), json!(notes));
    }
    if let Some(p) = opts.prerelease {
        body.insert("prerelease".into(), json!(p));
    }
    if opts.publish {
        body.insert("publish".into(), json!(true));
    }
    if opts.finalize {
        body.insert("finalize".into(), json!(true));
    }
    if body.is_empty() {
        return Err(OakError::InvalidArgument(
            "nothing to change: pass --title, --notes, --prerelease, --publish or --finalize"
                .to_string(),
        ));
    }
    if let Some(r) = opts.expected_revision {
        body.insert("expected_revision".into(), json!(r));
    }
    let tag = patch(target, name, Value::Object(body)).await?;
    if !tag.draft {
        target.cache_upsert(&tag);
    }
    Ok(MutationJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        outcome: "updated",
        recommended_next_commands: tag_next_commands(target, &tag),
        tag,
        uploads: Vec::new(),
    })
}

pub async fn delete(target: &Target, name: &str) -> Result<MutationJson> {
    let client = crate::http::api_client();
    let envelope: DeleteEnvelope = target
        .send(
            client.delete(target.tag_url(name, "")),
            crate::http::read_only_probe_timeout(),
        )
        .await?;
    target.cache_remove(name);
    Ok(MutationJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        outcome: "deleted",
        recommended_next_commands: vec![format!("oak release list{}", repo_flag(target))],
        tag: envelope.deleted,
        uploads: Vec::new(),
    })
}

// ---------------------------------------------------------------- push

#[derive(Debug, Serialize)]
pub struct PushedTagJson {
    pub name: String,
    pub commit: String,
    /// `created`, `unchanged` (already on the server), or `failed`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushJson {
    pub schema_version: u32,
    pub repo: String,
    pub tags: Vec<PushedTagJson>,
    pub recommended_next_commands: Vec<String>,
}

/// Publish every local-only tag (from `oak clone <git-url>`, or git-backed
/// `create_tag`) to the server, at the same commit. Their commits must
/// already be on the server's `main` (run `oak push` first).
pub async fn push(target: &Target) -> Result<PushJson> {
    let local = target.local.as_ref().ok_or_else(|| {
        OakError::InvalidArgument(
            "`oak release push` publishes this checkout's local tags; run it inside the checkout"
                .to_string(),
        )
    })?;
    let pending: Vec<LocalRelease> = local
        .list_releases()?
        .into_iter()
        .filter(|t| !t.synced)
        .collect();
    let client = crate::http::api_client();
    let mut rows = Vec::new();
    for tag in pending {
        let body = json!({ "name": tag.name, "commit": tag.commit_hash });
        let result: Result<TagEnvelope> = target
            .send(
                client.post(target.url("tags")).json(&body),
                crate::http::read_only_probe_timeout(),
            )
            .await;
        let row = match result {
            Ok(envelope) => {
                target.cache_upsert(&envelope.tag);
                PushedTagJson {
                    outcome: if envelope.created == Some(false) {
                        "unchanged"
                    } else {
                        "created"
                    },
                    name: tag.name,
                    commit: tag.commit_hash,
                    error: None,
                }
            }
            Err(e) => PushedTagJson {
                name: tag.name,
                commit: tag.commit_hash,
                outcome: "failed",
                error: Some(e.to_string()),
            },
        };
        output::info(&format!(
            "{} {}{}",
            row.outcome,
            row.name,
            row.error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
        rows.push(row);
    }
    let failed = rows.iter().filter(|r| r.outcome == "failed").count();
    if failed > 0 && failed == rows.len() {
        return Err(OakError::Server(format!(
            "none of {failed} local tag(s) could be published; push `main` first (`oak push`), then retry"
        )));
    }
    Ok(PushJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        tags: rows,
        recommended_next_commands: vec![format!("oak release list{}", repo_flag(target))],
    })
}

// ---------------------------------------------------------------- upload

#[derive(Debug, Serialize)]
pub struct UploadJson {
    pub name: String,
    pub path: String,
    pub blob_hash: String,
    pub size: u64,
    pub chunks: usize,
    /// False when the tag already had this exact asset.
    pub added: bool,
}

fn check_asset_file(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| {
        OakError::InvalidArgument(format!("cannot read asset {}: {e}", path.display()))
    })?;
    if !meta.is_file() {
        return Err(OakError::InvalidArgument(format!(
            "asset {} is not a regular file",
            path.display()
        )));
    }
    if meta.len() == 0 {
        return Err(OakError::InvalidArgument(format!(
            "asset {} is empty; release assets must have content",
            path.display()
        )));
    }
    Ok(())
}

fn asset_name_for(path: &Path, explicit: Option<&str>) -> Result<String> {
    if let Some(name) = explicit {
        return Ok(name.to_string());
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "cannot derive an asset name from {}; pass --name",
                path.display()
            ))
        })
}

fn guess_content_type(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let table: &[(&str, &str)] = &[
        (".tar.gz", "application/gzip"),
        (".tgz", "application/gzip"),
        (".gz", "application/gzip"),
        (".zip", "application/zip"),
        (".tar", "application/x-tar"),
        (".xz", "application/x-xz"),
        (".zst", "application/zstd"),
        (".json", "application/json"),
        (".txt", "text/plain"),
        (".md", "text/markdown"),
        (".sh", "text/x-shellscript"),
        (".pdf", "application/pdf"),
        (".dmg", "application/x-apple-diskimage"),
        (".deb", "application/vnd.debian.binary-package"),
        (".rpm", "application/x-rpm"),
        (".msi", "application/x-msi"),
        (".wasm", "application/wasm"),
        (".minisig", "text/plain"),
        (".sig", "application/pgp-signature"),
    ];
    table
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(_, ct)| *ct)
        .unwrap_or("application/octet-stream")
}

/// Chunk `path` exactly as push chunks a file (one self-chunk under the
/// large-file threshold, FastCDC above it), upload the chunks the server
/// lacks a page at a time, and hash the whole file in the same pass.
async fn upload_file_chunks(
    target: &Target,
    path: &Path,
) -> Result<(String, u64, Vec<oak_core::protocol::ChunkRefData>)> {
    target.require_linked()?;
    let size = std::fs::metadata(path)?.len();
    let retry = super::push::ChunkBusyRetryState::for_upload(size);
    let client = crate::http::api_client();
    let endpoint = format!("{}/{}", target.owner, target.name);
    let mut refs = Vec::new();

    if size < oak_core::LARGE_FILE_THRESHOLD {
        let data = std::fs::read(path)?;
        let hash = oak_core::hash_bytes(&data).to_string();
        refs.push(oak_core::protocol::ChunkRefData {
            hash: hash.clone(),
            offset: 0,
            size: data.len() as u32,
        });
        let page = std::collections::HashMap::from([(hash.clone(), data)]);
        super::push::upload_chunk_page(
            &client,
            &target.remote,
            &endpoint,
            target.token.as_deref(),
            page,
            &retry,
        )
        .await?;
        return Ok((hash, size, refs));
    }

    let file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut page: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut total = 0u64;
    for item in oak_core::chunking::stream_chunk_content(std::io::BufReader::new(file)) {
        let (info, data) = item?;
        hasher.update(&data);
        total += u64::from(info.length);
        refs.push(oak_core::protocol::ChunkRefData {
            hash: info.hash.to_string(),
            offset: info.offset,
            size: info.length,
        });
        page.entry(info.hash.to_string()).or_insert(data);
        if page.len() >= UPLOAD_PAGE_CHUNKS {
            super::push::upload_chunk_page(
                &client,
                &target.remote,
                &endpoint,
                target.token.as_deref(),
                std::mem::take(&mut page),
                &retry,
            )
            .await?;
        }
    }
    if !page.is_empty() {
        super::push::upload_chunk_page(
            &client,
            &target.remote,
            &endpoint,
            target.token.as_deref(),
            page,
            &retry,
        )
        .await?;
    }
    if total != size {
        return Err(OakError::InvalidArgument(format!(
            "{} changed while it was being uploaded ({size} bytes, then {total}); retry",
            path.display()
        )));
    }
    Ok((hasher.finalize().to_hex().to_string(), size, refs))
}

async fn upload_one(
    target: &Target,
    tag: &str,
    path: &Path,
    name: Option<&str>,
) -> Result<UploadJson> {
    check_asset_file(path)?;
    let asset_name = asset_name_for(path, name)?;
    let (hash, size, chunks) = upload_file_chunks(target, path).await?;
    let chunk_count = chunks.len();
    let client = crate::http::api_client();
    let body = json!({
        "name": asset_name,
        "content_type": guess_content_type(&asset_name),
        "blob": { "hash": hash, "size": size, "chunks": chunks },
    });
    let envelope: AssetEnvelope = target
        .send(
            client.post(target.tag_url(tag, "assets")).json(&body),
            REGISTER_TIMEOUT,
        )
        .await?;
    Ok(UploadJson {
        name: envelope.asset.name,
        path: path.display().to_string(),
        blob_hash: envelope.asset.blob_hash,
        size: envelope.asset.size,
        chunks: chunk_count,
        added: envelope.added,
    })
}

pub async fn upload(
    target: &Target,
    tag: &str,
    files: &[PathBuf],
    name: Option<&str>,
) -> Result<MutationJson> {
    if name.is_some() && files.len() != 1 {
        return Err(OakError::InvalidArgument(
            "--name renames a single asset; pass exactly one file with it".to_string(),
        ));
    }
    for path in files {
        check_asset_file(path)?;
    }
    let mut uploads = Vec::new();
    for path in files {
        let up = upload_one(target, tag, path, name).await?;
        output::info(&format!(
            "{} {} ({})",
            if up.added {
                "Uploaded"
            } else {
                "Already attached:"
            },
            up.name,
            human_bytes(up.size)
        ));
        uploads.push(up);
    }
    let tag = target.fetch_tag(tag).await?;
    if !tag.draft {
        target.cache_upsert(&tag);
    }
    Ok(MutationJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        outcome: "uploaded",
        recommended_next_commands: tag_next_commands(target, &tag),
        tag,
        uploads,
    })
}

pub async fn remove_asset(target: &Target, tag: &str, asset: &str) -> Result<MutationJson> {
    let client = crate::http::api_client();
    let _: AssetEnvelope = target
        .send(
            client.delete(target.tag_url(tag, &format!("assets/{}", urlencoding::encode(asset)))),
            crate::http::read_only_probe_timeout(),
        )
        .await?;
    let tag = target.fetch_tag(tag).await?;
    if !tag.draft {
        target.cache_upsert(&tag);
    }
    Ok(MutationJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        outcome: "removed",
        recommended_next_commands: tag_next_commands(target, &tag),
        tag,
        uploads: Vec::new(),
    })
}

// ---------------------------------------------------------------- download

#[derive(Debug, Serialize)]
pub struct DownloadedJson {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub blob_hash: String,
    /// True when an identical file was already at `path`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub already_present: bool,
}

#[derive(Debug, Serialize)]
pub struct DownloadJson {
    pub schema_version: u32,
    pub repo: String,
    pub tag: String,
    pub downloads: Vec<DownloadedJson>,
    pub recommended_next_commands: Vec<String>,
}

fn file_blob_hash(path: &Path) -> Result<Option<(String, u64)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok(Some((hasher.finalize().to_hex().to_string(), size)))
}

/// Stream one asset to `dest`, verifying its BLAKE3 against the blob hash
/// before the file appears (written to a temp file, then renamed).
async fn download_one(target: &Target, tag: &str, asset: &AssetWire, dest: &Path) -> Result<()> {
    target.require_linked()?;
    let client = crate::http::api_client();
    let url = target.tag_url(tag, &format!("assets/{}", urlencoding::encode(&asset.name)));
    let mut resp = target
        .auth(client.get(url))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default();
        return Err(target.status_error(status, serde_json::from_slice(&body).ok()));
    }
    let dir = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
    {
        hasher.update(&chunk);
        size += chunk.len() as u64;
        tmp.write_all(&chunk)?;
    }
    let got = hasher.finalize().to_hex().to_string();
    if size != asset.size || got != asset.blob_hash {
        return Err(OakError::Server(format!(
            "asset {} failed verification: expected {} bytes with hash {}, got {size} bytes with hash {got}",
            asset.name, asset.size, asset.blob_hash
        )));
    }
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| OakError::Io(e.error))?;
    Ok(())
}

pub async fn download(
    target: &Target,
    tag: &str,
    names: &[String],
    dir: &Path,
    force: bool,
) -> Result<DownloadJson> {
    let found = target.fetch_tag(tag).await?;
    if found.assets.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "{tag} has no assets to download"
        )));
    }
    let wanted: Vec<&AssetWire> = if names.is_empty() {
        found.assets.iter().collect()
    } else {
        names
            .iter()
            .map(|n| {
                found.assets.iter().find(|a| &a.name == n).ok_or_else(|| {
                    OakError::InvalidArgument(format!(
                        "{tag} has no asset named '{n}' (it has: {})",
                        found
                            .assets
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
            })
            .collect::<Result<_>>()?
    };
    std::fs::create_dir_all(dir)?;
    let mut downloads = Vec::new();
    for asset in wanted {
        if asset.name.contains('/')
            || asset.name.contains('\\')
            || asset.name.trim_start_matches('.').is_empty()
        {
            return Err(OakError::Server(format!(
                "refusing to write asset with unsafe name '{}'",
                asset.name
            )));
        }
        let dest = dir.join(&asset.name);
        match file_blob_hash(&dest)? {
            Some((hash, _)) if hash == asset.blob_hash => {
                downloads.push(DownloadedJson {
                    name: asset.name.clone(),
                    path: dest.display().to_string(),
                    size: asset.size,
                    blob_hash: asset.blob_hash.clone(),
                    already_present: true,
                });
                continue;
            }
            Some(_) if !force => {
                return Err(OakError::InvalidArgument(format!(
                    "{} exists with different content; pass --force to overwrite",
                    dest.display()
                )));
            }
            _ => {}
        }
        download_one(target, tag, asset, &dest).await?;
        output::info(&format!(
            "Downloaded {} ({})",
            dest.display(),
            human_bytes(asset.size)
        ));
        downloads.push(DownloadedJson {
            name: asset.name.clone(),
            path: dest.display().to_string(),
            size: asset.size,
            blob_hash: asset.blob_hash.clone(),
            already_present: false,
        });
    }
    Ok(DownloadJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        repo: target.full.clone(),
        tag: tag.to_string(),
        downloads,
        recommended_next_commands: vec![format!(
            "oak release view {}{}",
            quote(tag),
            repo_flag(target)
        )],
    })
}

pub fn print_mutation(result: &MutationJson) {
    let line = match result.outcome {
        "created" => format!(
            "Created release {} at {}",
            result.tag.name,
            &result.tag.commit[..result.tag.commit.len().min(12)]
        ),
        "unchanged" => format!("Release {} already exists", result.tag.name),
        "deleted" => format!("Deleted release {}", result.tag.name),
        "removed" => format!("Updated assets of {}", result.tag.name),
        "uploaded" => format!(
            "{} now has {} asset(s)",
            result.tag.name,
            result.tag.assets.len()
        ),
        _ => format!("Updated release {}", result.tag.name),
    };
    output::success(&line);
    if result.tag.draft && result.outcome != "deleted" {
        output::info(&format!(
            "Draft: only writers can see it. Publish with `oak release edit {} --publish`",
            quote(&result.tag.name)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_by_extension() {
        assert_eq!(guess_content_type("oak-x86_64.tar.gz"), "application/gzip");
        assert_eq!(guess_content_type("SHA256SUMS.txt"), "text/plain");
        assert_eq!(guess_content_type("oak"), "application/octet-stream");
    }

    #[test]
    fn quoting_for_recommended_commands() {
        assert_eq!(quote("v1.2.3"), "v1.2.3");
        assert_eq!(quote("release/2026"), "release/2026");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn local_round_trip_keeps_release_fields() {
        let local = LocalRelease {
            name: "v1".into(),
            commit_hash: "a".repeat(64),
            title: Some("One".into()),
            notes: None,
            prerelease: true,
            finalized: false,
            created_by: "zach".into(),
            created_at: "2026-11-02T00:00:00Z".into(),
            published_at: Some("2026-11-02T00:00:00Z".into()),
            revision: 3,
            synced: true,
            assets: vec![],
        };
        let wire = TagWire::from_local(&local);
        assert!(!wire.draft);
        assert_eq!(wire.to_local(), local);
    }

    #[test]
    fn upload_hash_matches_whole_file_hash_above_threshold() {
        // The streamed BLAKE3 over FastCDC chunks equals hashing the file.
        let data: Vec<u8> = (0..(oak_core::LARGE_FILE_THRESHOLD as usize + 123_457))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0u64;
        for item in oak_core::chunking::stream_chunk_content(&data[..]) {
            let (info, bytes) = item.unwrap();
            assert_eq!(info.offset, offset);
            offset += u64::from(info.length);
            hasher.update(&bytes);
        }
        assert_eq!(offset, data.len() as u64);
        assert_eq!(
            hasher.finalize().to_hex().to_string(),
            oak_core::hash_bytes(&data).to_string()
        );
    }
}
