//! `oak findings`: review findings on a branch — what a reviewer posted, and
//! what the merge gate refuses until each important one is fixed or resolved.
//!
//! Findings live on the server (`/api/:owner/:name/branches/:branch/findings`)
//! and are gated by the server's `review-findings` flag; while it is off the
//! routes 404 and these commands say so. Two ways to name the target:
//!
//! * from a checkout or mount: the remote, repo, current branch and its head
//!   come from the workspace;
//! * from anywhere (a CI job has only a git snapshot): `--repo OWNER/NAME
//!   --branch NAME`, with `--remote` (default `OAK_REMOTE`, then oak.space).
//!   A CI job authenticates with its task token: `OAK_API_KEY=$GITHUB_TOKEN`.
//!
//! Server error messages are our own structured refusals, so the `error` and
//! `code` fields are echoed; anything else in a body is never printed.

use std::path::Path;

use oak_core::{OakError, Repository, Result};
use serde::Serialize;
use serde_json::Value;

use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_POST_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
pub const RESOLUTION_REASONS: &[&str] = &["intentional", "wrong", "not_worth_it"];
pub const STATES: &[&str] = &["open", "fixed", "resolved"];

/// Where a findings command points, and with what credential.
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

    fn collection_url(&self) -> String {
        format!(
            "{}/api/{}/{}/branches/{}/findings",
            self.remote,
            urlencoding::encode(&self.owner),
            urlencoding::encode(&self.repo),
            super::branch_api_segment(&self.branch),
        )
    }

    fn finding_url(&self, id: i64, verb: &str) -> String {
        format!("{}/{id}/{verb}", self.collection_url())
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.header("authorization", format!("Bearer {token}")),
            None => req,
        }
    }

    /// `--repo/--branch` flags that re-target this exact branch, for
    /// recommended commands that must work outside the checkout too.
    fn target_flags(&self, explicit: bool) -> String {
        if explicit {
            format!(
                " --repo {}/{} --branch {}",
                self.owner,
                self.repo,
                shell_word(&self.branch)
            )
        } else {
            format!(" --branch {}", shell_word(&self.branch))
        }
    }
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:@".contains(&b))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Parse `fd-12` or `12`.
pub fn parse_id(raw: &str) -> Result<i64> {
    raw.strip_prefix("fd-")
        .unwrap_or(raw)
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            OakError::InvalidArgument(format!("finding id must look like fd-12, got {raw:?}"))
        })
}

fn is_full_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(OakError::Server(
            "findings response exceeds the 4 MiB budget".to_string(),
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
                "findings response exceeds the 4 MiB budget".to_string(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Turn a non-success response into an error, echoing only the server's own
/// `error`/`code` fields.
fn refusal(target: &Target, status: reqwest::StatusCode, body: &[u8]) -> OakError {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return OakError::AuthRequired {
            remote: target.remote.clone(),
            status: status.as_u16(),
        };
    }
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(Value::as_str)
        .map(|s| s.chars().take(500).collect::<String>());
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if status == reqwest::StatusCode::NOT_FOUND
        && matches!(message.as_deref(), None | Some("Not found"))
    {
        return OakError::Server(format!(
            "No findings for {}/{} branch '{}': review findings are not enabled for this \
             repository, or it isn't visible to you (HTTP 404)",
            target.owner, target.repo, target.branch
        ));
    }
    let detail = match (message, code) {
        (Some(message), Some(code)) => format!("{message} [{code}]"),
        (Some(message), None) => message,
        (None, _) => "no details".to_string(),
    };
    OakError::Server(format!(
        "Findings request refused (HTTP {status}): {detail}"
    ))
}

async fn send(target: &Target, req: reqwest::RequestBuilder) -> Result<Value> {
    let response = target
        .authed(req)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("findings request failed: {e}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(refusal(target, status, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| OakError::Server("invalid findings response".to_string()))?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(u64::from(SCHEMA_VERSION)) {
        return Err(OakError::Server(
            "unsupported findings response schema; upgrade the oak CLI".to_string(),
        ));
    }
    Ok(value)
}

fn location_text(finding: &Value) -> String {
    finding
        .get("locations")
        .and_then(Value::as_array)
        .map(|locations| {
            locations
                .iter()
                .filter_map(|l| {
                    let path = l.get("path")?.as_str()?;
                    let start = l.get("start_line").and_then(Value::as_u64);
                    let end = l.get("end_line").and_then(Value::as_u64);
                    Some(match (start, end) {
                        (Some(s), Some(e)) if e != s => format!("{path}:{s}-{e}"),
                        (Some(s), _) => format!("{path}:{s}"),
                        _ => path.to_string(),
                    })
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn finding_line(finding: &Value) -> String {
    let mut line = format!(
        "{:<8} {:<12} {:<9} {:<9} {}",
        str_field(finding, "display_id"),
        str_field(finding, "severity"),
        str_field(finding, "source"),
        str_field(finding, "state"),
        str_field(finding, "title"),
    );
    let locations = location_text(finding);
    if !locations.is_empty() {
        line.push_str(&format!("\n{:<8} {locations}", ""));
    }
    line
}

fn counts_summary(value: &Value) -> String {
    let count = |key: &str| {
        value
            .get("counts")
            .and_then(|c| c.get(key))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let open = count("open_important") + count("open_other");
    format!(
        "{open} open ({} blocking), {} fixed, {} resolved{}",
        count("open_important"),
        count("fixed"),
        count("resolved"),
        if value.get("enforced").and_then(Value::as_bool) == Some(false) {
            "; the merge gate is not enforced for this repo"
        } else {
            ""
        }
    )
}

/// `oak findings list`
pub async fn list(path: &Path, args: &TargetArgs, state: Option<&str>, json: bool) -> Result<()> {
    if let Some(state) = state {
        if !STATES.contains(&state) {
            return Err(OakError::InvalidArgument(format!(
                "--state must be one of {}",
                STATES.join(", ")
            )));
        }
    }
    let target = Target::resolve(path, args)?;
    let mut req = crate::http::api_client().get(target.collection_url());
    if let Some(state) = state {
        req = req.query(&[("state", state)]);
    }
    let mut value = send(&target, req).await?;
    let blocking = value.get("blocking").and_then(Value::as_i64).unwrap_or(0);
    let enforced = value.get("enforced").and_then(Value::as_bool) == Some(true);
    let next: Vec<String> = if blocking > 0 && enforced {
        vec![format!(
            "oak findings list{} --state open --json",
            target.target_flags(args.repo.is_some())
        )]
    } else {
        Vec::new()
    };
    if json {
        value["recommended_next_commands"] = serde_json::json!(next);
        return output::print_json(&value);
    }
    output::print_line(&format!(
        "{}/{} {}: {}",
        target.owner,
        target.repo,
        target.branch,
        counts_summary(&value)
    ));
    for finding in value
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        output::print_line(&finding_line(finding));
    }
    Ok(())
}

/// What `post` reads from `--file`: the POST body, minus or matching
/// `expected_head`.
fn post_body(raw: &[u8], expected_head: &str) -> Result<Value> {
    let mut body: Value = serde_json::from_slice(raw)
        .map_err(|e| OakError::InvalidArgument(format!("findings file is not valid JSON: {e}")))?;
    let object = body.as_object_mut().ok_or_else(|| {
        OakError::InvalidArgument(
            "findings file must be a JSON object with `findings` and/or `fixed_keys`".to_string(),
        )
    })?;
    match object.get("expected_head").and_then(Value::as_str) {
        Some(head) if head != expected_head => {
            return Err(OakError::InvalidArgument(format!(
                "the findings file says expected_head {head}, but this post is for {expected_head}"
            )))
        }
        _ => {}
    }
    object.insert(
        "expected_head".to_string(),
        Value::String(expected_head.to_string()),
    );
    if !object.contains_key("findings") && !object.contains_key("fixed_keys") {
        return Err(OakError::InvalidArgument(
            "findings file has neither `findings` nor `fixed_keys`".to_string(),
        ));
    }
    Ok(body)
}

fn read_input(file: &str) -> Result<Vec<u8>> {
    let bytes = if file == "-" {
        use std::io::Read as _;
        let mut buf = Vec::new();
        std::io::stdin()
            .take(MAX_POST_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(OakError::Io)?;
        buf
    } else {
        std::fs::read(file).map_err(OakError::Io)?
    };
    if bytes.len() > MAX_POST_BYTES {
        return Err(OakError::InvalidArgument(
            "findings file exceeds 8 MiB".to_string(),
        ));
    }
    Ok(bytes)
}

/// `oak findings post`
pub async fn post(
    path: &Path,
    args: &TargetArgs,
    file: &str,
    expected_head: Option<&str>,
    json: bool,
) -> Result<()> {
    let target = Target::resolve(path, args)?;
    let head = match expected_head.or(target.local_head.as_deref()) {
        Some(head) => head.to_string(),
        None => {
            return Err(OakError::InvalidArgument(
                "pass --expected-head HASH: the exact commit that was reviewed".to_string(),
            ))
        }
    };
    if !is_full_hash(&head) {
        return Err(OakError::InvalidArgument(
            "--expected-head must be a full 64-character lowercase commit hash".to_string(),
        ));
    }
    let body = post_body(&read_input(file)?, &head)?;
    let req = crate::http::api_client()
        .post(target.collection_url())
        .json(&body);
    let mut value = send(&target, req).await?;
    let blocking = value.get("blocking").and_then(Value::as_i64).unwrap_or(0);
    if json {
        if value.get("recommended_next_commands").is_none() {
            value["recommended_next_commands"] = serde_json::json!([]);
        }
        return output::print_json(&value);
    }
    let mut created = 0;
    let mut reopened = 0;
    let mut duplicate = 0;
    for posted in value
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match str_field(posted, "disposition") {
            "created" => created += 1,
            "reopened" => reopened += 1,
            _ => duplicate += 1,
        }
    }
    let fixed = value
        .get("fixed")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    output::success(&format!(
        "Posted to {}: {created} new, {reopened} reopened, {duplicate} already known, {fixed} marked fixed; {blocking} blocking",
        target.branch
    ));
    Ok(())
}

#[derive(Serialize)]
struct ResolveBody<'a> {
    reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<i32>,
}

fn print_transition(value: Value, json: bool, verb: &str) -> Result<()> {
    if json {
        let mut value = value;
        value["recommended_next_commands"] = serde_json::json!([]);
        return output::print_json(&value);
    }
    let finding = value.get("finding").cloned().unwrap_or(Value::Null);
    output::success(&format!(
        "{verb} {} (revision {}); {} blocking",
        str_field(&finding, "display_id"),
        finding.get("revision").and_then(Value::as_i64).unwrap_or(0),
        value.get("blocking").and_then(Value::as_i64).unwrap_or(0),
    ));
    Ok(())
}

/// `oak findings resolve`
pub async fn resolve(
    path: &Path,
    args: &TargetArgs,
    id: &str,
    reason: &str,
    note: Option<&str>,
    expected_revision: Option<i32>,
    json: bool,
) -> Result<()> {
    let id = parse_id(id)?;
    if !RESOLUTION_REASONS.contains(&reason) {
        return Err(OakError::InvalidArgument(format!(
            "--reason must be one of {}",
            RESOLUTION_REASONS.join(", ")
        )));
    }
    let target = Target::resolve(path, args)?;
    let req = crate::http::api_client()
        .post(target.finding_url(id, "resolve"))
        .json(&ResolveBody {
            reason,
            note,
            expected_revision,
        });
    print_transition(send(&target, req).await?, json, "Resolved")
}

/// `oak findings reopen`
pub async fn reopen(
    path: &Path,
    args: &TargetArgs,
    id: &str,
    expected_revision: Option<i32>,
    json: bool,
) -> Result<()> {
    let id = parse_id(id)?;
    let target = Target::resolve(path, args)?;
    let req = crate::http::api_client()
        .post(target.finding_url(id, "reopen"))
        .json(&serde_json::json!({ "expected_revision": expected_revision }));
    print_transition(send(&target, req).await?, json, "Reopened")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_both_forms() {
        assert_eq!(parse_id("fd-12").unwrap(), 12);
        assert_eq!(parse_id("12").unwrap(), 12);
        assert!(parse_id("fd-0").is_err());
        assert!(parse_id("fb-12").is_err());
    }

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

    #[test]
    fn post_body_fills_and_fences_the_reviewed_head() {
        let head = "a".repeat(64);
        let body = post_body(br#"{"findings": []}"#, &head).unwrap();
        assert_eq!(body["expected_head"], head);

        let matching = format!(r#"{{"expected_head": "{head}", "fixed_keys": ["k"]}}"#);
        assert!(post_body(matching.as_bytes(), &head).is_ok());

        let other = format!(
            r#"{{"expected_head": "{}", "findings": []}}"#,
            "b".repeat(64)
        );
        assert!(post_body(other.as_bytes(), &head).is_err());
        assert!(post_body(b"[]", &head).is_err());
        assert!(post_body(b"{}", &head).is_err());
    }

    #[test]
    fn branch_names_are_quoted_in_recommended_commands() {
        assert_eq!(shell_word("feature"), "feature");
        assert_eq!(shell_word("contrib/bob/fix"), "contrib/bob/fix");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }
}
