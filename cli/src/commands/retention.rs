//! `oak retention`: keep only the newest versions of files under a directory.
//!
//! Rules live on the server (`/api/:owner/:name/retention`, the `retention`
//! flag): a directory and how many versions of each file under it keep their
//! content. Older versions keep their commits and history, but the server
//! stops serving their bytes and reclaims the storage after a grace window.
//! Content a branch head, a release, or a file outside the rule still uses is
//! always kept. Changing a rule needs an owner or admin of the repo's
//! organization.
//!
//! Clients see expired content as `missing_content` with reason
//! `retention_expired` on pull (see [`super::known_loss`]): clone and pull
//! skip those versions and say so, and the current tree is never affected.

use std::time::Duration;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::access::RepoTarget;
use super::branch_target::bounded_body;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// One rule, as the server describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    /// The directory (`""` is the whole repo).
    pub path: String,
    pub keep_versions: i64,
    #[serde(default)]
    pub expired_blobs: i64,
    #[serde(default)]
    pub expired_bytes: i64,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    #[serde(default)]
    pub run_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub expired_blobs: i64,
    #[serde(default)]
    pub expired_bytes: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ListWire {
    rules: Vec<Rule>,
    #[serde(default)]
    status: Status,
    #[serde(default)]
    can_edit: bool,
}

#[derive(Debug, Serialize)]
pub struct ListJson {
    pub schema_version: u32,
    pub repo: String,
    pub rules: Vec<Rule>,
    pub status: Status,
    pub can_edit: bool,
    /// Exact invocations only; the way to add a rule needs a directory and
    /// a count, so it is given as `set_command_template` instead.
    pub recommended_next_commands: Vec<String>,
    /// How to add a rule (`DIR` and `N` are placeholders). Present when the
    /// caller can edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_command_template: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChangeJson {
    pub schema_version: u32,
    pub repo: String,
    pub action: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_versions: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_keep_versions: Option<i64>,
    pub recommended_next_commands: Vec<String>,
}

/// `""` (the whole repo) shows as `/`, a directory as `dir/`.
pub fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        format!("{path}/")
    }
}

/// Trim slashes the way the server normalizes a rule directory.
fn normalize(path: &str) -> String {
    path.trim().trim_matches('/').to_string()
}

fn error_for(
    target: &RepoTarget,
    status: reqwest::StatusCode,
    body: &[u8],
    writing: bool,
) -> OakError {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return OakError::AuthRequired {
            remote: target.remote.clone(),
            status: status.as_u16(),
        };
    }
    if status == reqwest::StatusCode::NOT_FOUND && !writing {
        return OakError::Server(format!(
            "{} has no retention settings you can see: retention isn't on for it, or it isn't \
             visible to you (HTTP 404)",
            target.slug()
        ));
    }
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"));
    OakError::Server(message)
}

async fn send(target: &RepoTarget, req: reqwest::RequestBuilder, writing: bool) -> Result<Vec<u8>> {
    let response = req
        .send()
        .await
        .map_err(|e| OakError::Http(format!("retention on {}: {e}", target.slug())))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(error_for(target, status, &body, writing));
    }
    Ok(body)
}

async fn fetch(target: &RepoTarget) -> Result<ListWire> {
    let body = send(
        target,
        target.request(
            reqwest::Method::GET,
            target.url("retention"),
            REQUEST_TIMEOUT,
        ),
        false,
    )
    .await?;
    serde_json::from_slice(&body)
        .map_err(|e| OakError::Server(format!("unreadable retention response: {e}")))
}

fn human_bytes(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = b;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `oak retention [list]`.
pub async fn list(target: RepoTarget, json: bool) -> Result<()> {
    let wire = fetch(&target).await?;
    let slug = target.slug();
    if json {
        // A template, not a runnable command (DIR and N are placeholders),
        // so it is not offered as a recommended command.
        let set_command_template = wire
            .can_edit
            .then(|| format!("oak retention set DIR --keep N --repo {slug}"));
        return output::print_json(&ListJson {
            schema_version: SCHEMA_VERSION,
            repo: slug,
            rules: wire.rules,
            status: wire.status,
            can_edit: wire.can_edit,
            recommended_next_commands: Vec::new(),
            set_command_template,
        });
    }
    if wire.rules.is_empty() {
        output::print_line(&format!(
            "{slug} has no retention rules: every version of every file keeps its content."
        ));
    }
    let mut rules = wire.rules.clone();
    rules.sort_by(|a, b| a.path.cmp(&b.path));
    for rule in &rules {
        let expired = if rule.expired_blobs == 0 {
            String::new()
        } else {
            format!(
                "  ({} older version(s) expired, {})",
                rule.expired_blobs,
                human_bytes(rule.expired_bytes)
            )
        };
        output::print_line(&format!(
            "{:<32} keep {}{expired}",
            display_path(&rule.path),
            rule.keep_versions
        ));
    }
    let status = &wire.status;
    match (&status.last_outcome, &status.last_error) {
        (Some(outcome), Some(error)) if outcome == "error" => {
            output::warning(&format!("The last retention run failed: {error}"))
        }
        _ => {}
    }
    if status.run_requested {
        output::info("A recompute is queued.");
    }
    Ok(())
}

/// `oak retention set DIR --keep N`.
pub async fn set(target: RepoTarget, path: &str, keep: i64, json: bool) -> Result<()> {
    if keep < 1 {
        return Err(OakError::InvalidArgument(
            "--keep must be at least 1 (the current version is always kept)".to_string(),
        ));
    }
    let path = normalize(path);
    let req = target
        .request(
            reqwest::Method::PUT,
            target.url("retention"),
            REQUEST_TIMEOUT,
        )
        .json(&serde_json::json!({ "path": path, "keep_versions": keep }));
    let body = send(&target, req, true).await?;
    #[derive(Deserialize)]
    struct Wire {
        path: String,
        keep_versions: i64,
        #[serde(default)]
        previous_keep_versions: Option<i64>,
    }
    let wire: Wire = serde_json::from_slice(&body)
        .map_err(|e| OakError::Server(format!("unreadable retention response: {e}")))?;
    let slug = target.slug();
    if json {
        return output::print_json(&ChangeJson {
            schema_version: SCHEMA_VERSION,
            repo: slug.clone(),
            action: "set",
            path: wire.path,
            keep_versions: Some(wire.keep_versions),
            previous_keep_versions: wire.previous_keep_versions,
            recommended_next_commands: vec![format!("oak retention --repo {slug} --json")],
        });
    }
    output::success(&format!(
        "{} keeps the newest {} version(s) of each file; older content expires on the next run",
        display_path(&wire.path),
        wire.keep_versions
    ));
    Ok(())
}

/// `oak retention remove DIR`.
pub async fn remove(target: RepoTarget, path: &str, json: bool) -> Result<()> {
    let path = normalize(path);
    let url = format!(
        "{}?path={}",
        target.url("retention"),
        urlencoding::encode(&path)
    );
    send(
        &target,
        target.request(reqwest::Method::DELETE, url, REQUEST_TIMEOUT),
        true,
    )
    .await?;
    let slug = target.slug();
    if json {
        return output::print_json(&ChangeJson {
            schema_version: SCHEMA_VERSION,
            repo: slug.clone(),
            action: "remove",
            path,
            keep_versions: None,
            previous_keep_versions: None,
            recommended_next_commands: vec![format!("oak retention --repo {slug} --json")],
        });
    }
    output::success(&format!(
        "Removed the rule for {}; versions it expired come back unless their storage was already reclaimed",
        display_path(&path)
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_and_normalizes_directories() {
        assert_eq!(display_path(""), "/");
        assert_eq!(display_path("build/out"), "build/out/");
        assert_eq!(normalize("/build/out/"), "build/out");
        assert_eq!(normalize(" / "), "");
    }

    #[test]
    fn reads_a_server_listing() {
        let wire: ListWire = serde_json::from_value(serde_json::json!({
            "rules": [{"path": "build", "keep_versions": 3, "expired_blobs": 2,
                       "expired_bytes": 2048, "updated_at": "2026-11-08T00:00:00Z"}],
            "status": {"run_requested": false, "expired_blobs": 2, "expired_bytes": 2048},
            "can_edit": true
        }))
        .unwrap();
        assert_eq!(wire.rules[0].keep_versions, 3);
        assert!(wire.can_edit);
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(12), "12 B");
    }
}
