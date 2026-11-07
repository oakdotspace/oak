//! `oak ls --remote` and `oak grep --remote`: look at paths outside this
//! checkout (another project, say) without widening it.
//!
//! Both are read-only and never touch refs, branch rows or working files.
//! They go through the server's permission-checked routes, so a path the
//! caller can't read is simply absent. `oak cat --remote` lives with
//! `oak file inspect --remote`, whose bytes are verified against the pinned
//! manifest; these two are listings and search results, which the server
//! redacts rather than proves, so their JSON says `verified: false`.

use std::path::Path;

use oak_core::{Hash, OakError, Result};
use serde::{Deserialize, Serialize};

use crate::commands::branch::{RemoteIdentity, RemoteWorkspace};
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
/// Bound on one listing or search response.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The commit `revision` names on the remote: a full hash as is, otherwise a
/// branch name resolved through the branch list. Returns how it was found.
async fn resolve_revision(remote: &RemoteIdentity, revision: &str) -> Result<(Hash, &'static str)> {
    if revision.len() == 64 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok((
            Hash::from_hex(&revision.to_ascii_lowercase())?,
            "explicit_commit",
        ));
    }
    if revision.len() >= 4 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OakError::InvalidArgument(format!(
            "--rev {revision}: abbreviated hashes are ambiguous remotely; pass a branch name or the full 64-character commit hash"
        )));
    }
    let branches = crate::commands::branch::fetch_remote_branches(remote).await?;
    let branch = branches
        .iter()
        .find(|b| b.name == revision)
        .ok_or_else(|| OakError::BranchNotFound(revision.to_string()))?;
    let head = branch
        .head
        .as_deref()
        .ok_or_else(|| OakError::Server(format!("remote branch '{revision}' has no head")))?;
    Ok((Hash::from_hex(head)?, "remote_branch_list"))
}

/// `{remote}/api/{owner}/{repo}/{segments...}`, each segment encoded.
fn api_url(remote: &RemoteIdentity, segments: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&remote.remote_url)
        .map_err(|e| OakError::InvalidArgument(format!("remote URL: {e}")))?;
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|()| OakError::InvalidArgument("remote URL can't take a path".into()))?
        .pop_if_empty()
        .extend(["api", remote.owner.as_str(), remote.repo_name.as_str()])
        .extend(segments);
    Ok(url)
}

/// GET `url` with the remote's credential, bounded; 404 becomes `None`.
async fn get_bounded(
    remote: &RemoteIdentity,
    url: reqwest::Url,
    what: &str,
) -> Result<Option<Vec<u8>>> {
    let client = crate::http::api_client();
    let timeout = crate::http::read_only_probe_timeout();
    let mut request = client.get(url).timeout(timeout);
    if let Some(token) = remote.token.as_deref() {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let shown = crate::http::redact_url_userinfo(&remote.remote_url);
    let transport = |error: reqwest::Error| {
        OakError::Http(format!(
            "{what} from {shown}: {}",
            crate::http::transport_failure_reason(&error, timeout)
        ))
    };
    let mut resp = request.send().await.map_err(transport)?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(OakError::AuthRequired {
            remote: shown.clone(),
            status: status.as_u16(),
        });
    }
    if !status.is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(OakError::Server(format!(
                "{what} exceeds the {MAX_RESPONSE_BYTES}-byte client bound"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

fn normalize_dir(dir: &str) -> Result<String> {
    let dir = dir.replace('\\', "/");
    let dir = dir.trim_matches('/');
    if dir
        .split('/')
        .any(|part| part == "." || part == ".." || (part.is_empty() && !dir.is_empty()))
    {
        return Err(OakError::InvalidArgument(format!(
            "'{dir}': use a repo-relative path without `.` or `..`"
        )));
    }
    Ok(dir.to_string())
}

// ---------------------------------------------------------------------------
// oak ls --remote
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone)]
pub struct ListedEntry {
    pub name: String,
    pub path: String,
    /// `blob` or `tree`.
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub blob_hash: String,
}

#[derive(Serialize)]
struct LsJson {
    schema_version: u32,
    kind: &'static str,
    remote: String,
    revision: String,
    head_source: &'static str,
    commit: String,
    path: String,
    /// Listings are redacted by the server, not hash-verified.
    verified: bool,
    entries: Vec<ListedEntry>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    recommended_next_commands: Vec<String>,
}

pub async fn ls_remote(
    cwd: &Path,
    revision: &str,
    dir: &str,
    max_entries: usize,
    json: bool,
) -> Result<()> {
    let workspace = RemoteWorkspace::open(cwd)?;
    let remote = &workspace.remote;
    let dir = normalize_dir(dir)?;
    let (commit, head_source) = resolve_revision(remote, revision).await?;
    let mut segments = vec!["tree", commit.as_str()];
    segments.extend(dir.split('/').filter(|s| !s.is_empty()));
    let body = get_bounded(remote, api_url(remote, &segments)?, "remote listing")
        .await?
        .ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "'{}' is not a directory you can read at {revision}",
                if dir.is_empty() { "." } else { &dir }
            ))
        })?;
    let mut entries: Vec<ListedEntry> = serde_json::from_slice(&body)
        .map_err(|_| OakError::Server("the server returned an unreadable listing".into()))?;
    entries.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    let truncated = entries.len() > max_entries;
    entries.truncate(max_entries);
    if !json {
        for e in &entries {
            if e.kind == "tree" {
                output::print_line(&format!("{}/", e.name));
            } else {
                output::print_line(&e.name);
            }
        }
        if truncated {
            output::info(&format!(
                "(first {max_entries} entries; raise --max-entries)"
            ));
        }
        return Ok(());
    }
    let mut recommended_next_commands = Vec::new();
    if let Some(file) = entries.iter().find(|e| e.kind == "blob") {
        recommended_next_commands.push(format!(
            "oak cat --remote {} --rev {revision} --json",
            crate::commands::review::shell_quote_path(&file.path)
        ));
    }
    if !dir.is_empty() {
        recommended_next_commands.push(format!(
            "oak project which {} --json",
            crate::commands::review::shell_quote_path(&dir)
        ));
    }
    output::print_json(&LsJson {
        schema_version: SCHEMA_VERSION,
        kind: "remote_listing",
        remote: format!("{}/{}", remote.owner, remote.repo_name),
        revision: revision.to_string(),
        head_source,
        commit: commit.to_string(),
        path: dir,
        verified: false,
        entries,
        truncated,
        recommended_next_commands,
    })
}

// ---------------------------------------------------------------------------
// oak grep --remote
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone)]
pub struct GrepLine {
    pub line: u32,
    /// Alternating plain / match pieces (`parts[0]` plain, `parts[1]` a
    /// match, …), so no byte offsets need translating.
    pub parts: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct GrepFile {
    pub path: String,
    /// Matches in the file (including lines past those listed).
    pub total: usize,
    pub lines: Vec<GrepLine>,
}

#[derive(Serialize)]
struct GrepJson {
    schema_version: u32,
    kind: &'static str,
    remote: String,
    /// Search runs over `main`'s tree.
    revision: &'static str,
    pattern: String,
    regex: bool,
    case_sensitive: bool,
    /// Path prefixes the results were limited to (empty: everywhere).
    paths: Vec<String>,
    /// Results are filtered by the server, not hash-verified.
    verified: bool,
    files: Vec<GrepFile>,
    matches: usize,
    /// The server or this client stopped early (result cap or time budget).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    limited: bool,
    recommended_next_commands: Vec<String>,
}

#[derive(Deserialize)]
struct SearchEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    total: usize,
    #[serde(default)]
    lines: Vec<GrepLine>,
    #[serde(default)]
    limited: bool,
    #[serde(default)]
    message: String,
}

fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

pub struct GrepOptions {
    pub pattern: String,
    pub paths: Vec<String>,
    pub regex: bool,
    pub case_sensitive: bool,
    pub max_files: usize,
}

pub async fn grep_remote(cwd: &Path, opts: GrepOptions, json: bool) -> Result<bool> {
    let workspace = RemoteWorkspace::open(cwd)?;
    let remote = &workspace.remote;
    let prefixes: Vec<String> = opts
        .paths
        .iter()
        .map(|p| normalize_dir(p))
        .collect::<Result<_>>()?;
    let mut url = api_url(remote, &["search"])?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("q", &opts.pattern);
        if opts.regex {
            q.append_pair("regex", "1");
        }
        if opts.case_sensitive {
            q.append_pair("case", "1");
        }
    }
    let body = get_bounded(remote, url, "remote search")
        .await?
        .ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "{}/{} has no searchable main (or this server has no search API)",
                remote.owner, remote.repo_name
            ))
        })?;
    let mut files = Vec::new();
    let mut limited = false;
    for line in body.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(event) = serde_json::from_slice::<SearchEvent>(line) else {
            continue;
        };
        match event.kind.as_str() {
            "file" if prefixes.is_empty() || prefixes.iter().any(|p| covers(p, &event.path)) => {
                if files.len() >= opts.max_files {
                    limited = true;
                    continue;
                }
                files.push(GrepFile {
                    path: event.path,
                    total: event.total,
                    lines: event.lines,
                });
            }
            "done" => limited |= event.limited,
            "error" => return Err(OakError::InvalidArgument(event.message)),
            _ => {}
        }
    }
    let matches = files.iter().map(|f| f.total).sum();
    let found = !files.is_empty();
    if !json {
        for f in &files {
            for l in &f.lines {
                output::print_line(&format!("{}:{}:{}", f.path, l.line, l.parts.concat()));
            }
        }
        if limited {
            output::info("(results were limited; narrow the pattern or the paths)");
        }
        return Ok(found);
    }
    let recommended_next_commands = files
        .first()
        .map(|f| {
            vec![format!(
                "oak cat --remote {} --json",
                crate::commands::review::shell_quote_path(&f.path)
            )]
        })
        .unwrap_or_default();
    output::print_json(&GrepJson {
        schema_version: SCHEMA_VERSION,
        kind: "remote_search",
        remote: format!("{}/{}", remote.owner, remote.repo_name),
        revision: "main",
        pattern: opts.pattern,
        regex: opts.regex,
        case_sensitive: opts.case_sensitive,
        paths: prefixes,
        verified: false,
        files,
        matches,
        limited,
        recommended_next_commands,
    })?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_are_normalized_and_dot_parts_refused() {
        assert_eq!(normalize_dir("/libs/auth/").unwrap(), "libs/auth");
        assert_eq!(normalize_dir("").unwrap(), "");
        assert!(normalize_dir("libs/../secret").is_err());
        assert!(normalize_dir("./libs").is_err());
        assert!(normalize_dir("libs//auth").is_err());
    }

    #[test]
    fn api_urls_encode_each_segment() {
        let remote = RemoteIdentity {
            remote_url: "https://oak.space/".to_string(),
            owner: "acme".to_string(),
            repo_name: "co".to_string(),
            token: None,
        };
        let url = api_url(&remote, &["tree", "abc", "a b", "c#d"]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://oak.space/api/acme/co/tree/abc/a%20b/c%23d"
        );
    }
}
