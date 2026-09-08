//! `oak ci` — the CLI surface over the server's native-CI runs API.
//!
//! Merges onto main are CI-gated server-side (`oak merge` fails with
//! `HTTP 412` while CI is running or after it failed), and until this
//! command group existed the only way to see *why* — or to re-run a run
//! that died for infra reasons — was to read `~/.oak/credentials` and
//! curl the API by hand. The subcommands map 1:1 onto the endpoints:
//!
//! - `oak ci runs`        → `GET  /api/:owner/:repo/ci/runs?limit=N`
//! - `oak ci status`      → same list, filtered client-side to the current
//!   branch head (the commit the merge gate checks)
//! - `oak ci logs <id>`   → `GET  /api/:owner/:repo/ci/runs/:id` (includes
//!   per-step logs)
//! - `oak ci rerun <id>`  → `POST /api/:owner/:repo/ci/runs` with
//!   `{"workflow", "branch", "commit", "event": "manual"}`
//! - `oak ci cancel <id>` → exact-run preflight, then
//!   `POST /api/:owner/:repo/ci/runs/:id/cancel` for ordinary CI only
//!
//! ## Defensive JSON parsing (fb-30)
//!
//! The runs API can embed raw control characters (ANSI escapes, NULs) in
//! step `logs` fields, which is invalid strict JSON and makes
//! `serde_json` bail. Until the server-side fix lands, [`parse_json_lenient`]
//! retries a failed parse after rewriting raw control characters *inside
//! string literals* to their `\uXXXX` escapes (structural whitespace
//! between tokens is left alone), so `oak ci` never crashes on a log line.

use std::path::Path;

use oak_core::{MetadataKey, OakError, Repository, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::output;

const SCHEMA_VERSION: u32 = 1;

/// Default number of runs shown by `oak ci runs`.
pub const DEFAULT_RUNS_LIMIT: usize = 20;

/// How many recent runs to scan when looking for the current head's run.
/// CI dispatches at most a couple of runs per push, so the head's run is
/// always near the top of the reverse-chronological list.
const STATUS_SCAN_LIMIT: usize = 50;

const CI_MUTATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CI_RECEIPT_MAX_BYTES: usize = 1024 * 1024;

async fn bounded_ci_body(mut response: reqwest::Response) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > CI_RECEIPT_MAX_BYTES as u64)
    {
        return Err(OakError::Server(
            "CI response exceeds the 1 MiB receipt budget".into(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
    {
        if chunk.len() > CI_RECEIPT_MAX_BYTES.saturating_sub(bytes.len()) {
            return Err(OakError::Server(
                "CI response exceeds the 1 MiB receipt budget".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| OakError::Server("CI response is not UTF-8".into()))
}

fn valid_run_ids(ids: &[u64]) -> bool {
    !ids.is_empty()
        && ids.iter().all(|id| *id > 0 && *id <= i64::MAX as u64)
        && ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TriggerReceipt {
    pub protocol: String,
    pub branch: String,
    pub commit_hash: String,
    pub run_ids: Vec<u64>,
    pub replayed: bool,
}

pub struct CiDispatchReceipt {
    pub run: CiRun,
    pub run_ids: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct CancelReceipt {
    pub run_id: u64,
    pub commit_hash: String,
    pub event: String,
    pub outcome: &'static str,
    pub control_plane_cancellation_recorded: bool,
    pub execution_stop: &'static str,
}

// ---------------------------------------------------------------------------
// Response types — parsed leniently (every field defaulted, unknown fields
// preserved via `extra`) per the append-only schema policy, and re-serialized
// for `--json` output.
// ---------------------------------------------------------------------------

/// One CI run as returned by the server's runs API. The list endpoint omits
/// `jobs`; the single-run endpoint includes them (with per-step logs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CiRun {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub workflow_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_path: Option<String>,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub commit_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggered_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs: Option<Vec<CiJob>>,
    /// Forward-compat: any fields this CLI doesn't know yet pass through
    /// to `--json` output untouched.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CiJob {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub steps: Vec<CiStep>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CiStep {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl CiRun {
    /// The run's state as the merge gate sees it.
    pub fn gate_state(&self) -> CiGateState {
        match self.conclusion.as_deref() {
            Some("success") => CiGateState::Success,
            Some(_) => CiGateState::Failure,
            None => {
                // No conclusion. A completed run without one (or with a
                // top-level error) is a failure; anything else is still
                // in flight.
                if self.status == "completed" || self.error.is_some() {
                    CiGateState::Failure
                } else {
                    CiGateState::Running
                }
            }
        }
    }

    /// Wall-clock duration between `started_at` and `finished_at`, formatted
    /// compactly ("6m22s"), or "-" when the run hasn't finished.
    fn duration_display(&self) -> String {
        let (Some(start), Some(end)) = (&self.started_at, &self.finished_at) else {
            return "-".to_string();
        };
        match (
            chrono::DateTime::parse_from_rfc3339(start),
            chrono::DateTime::parse_from_rfc3339(end),
        ) {
            (Ok(s), Ok(e)) => format_duration_secs((e - s).num_seconds()),
            _ => "-".to_string(),
        }
    }

    /// One compact human line: `#170  ci  merge  main@04170511  success  6m22s`.
    fn summary_line(&self) -> String {
        let commit = &self.commit_hash[..self.commit_hash.len().min(12)];
        let outcome = self
            .conclusion
            .clone()
            .unwrap_or_else(|| self.status.clone());
        format!(
            "#{:<5} {:<10} {:<7} {}@{}  {}  {}",
            self.id,
            self.workflow_name,
            self.event,
            self.branch,
            commit,
            outcome,
            self.duration_display(),
        )
    }
}

/// What a run means for the merge gate. Ordered so scripts can branch on
/// `oak ci status`'s exit code: 0 success, 1 failure (or no runs), 3 still
/// running (retry later — same "retryable" meaning as the repo-wide code 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiGateState {
    Success,
    Failure,
    Running,
}

impl CiGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CiGateState::Success => "success",
            CiGateState::Failure => "failure",
            CiGateState::Running => "running",
        }
    }

    /// Exit code for `oak ci status` (see the enum docs).
    pub fn exit_code(self) -> i32 {
        match self {
            CiGateState::Success => 0,
            CiGateState::Failure => 1,
            CiGateState::Running => 3,
        }
    }
}

pub(crate) fn format_duration_secs(total: i64) -> String {
    if total < 0 {
        return "-".to_string();
    }
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// Defensive JSON parsing (see module docs / fb-30)
// ---------------------------------------------------------------------------

/// Parse `body` as `T`, retrying once with raw control characters inside
/// string literals rewritten to `\uXXXX` escapes. The retry only exists for
/// the server bug where step `logs` carry raw ANSI/control bytes (invalid
/// strict JSON); the error reported on double failure is the *first* parse
/// error, which points at the server's actual output.
pub fn parse_json_lenient<T: DeserializeOwned>(body: &str) -> Result<T> {
    match serde_json::from_str(body) {
        Ok(v) => Ok(v),
        Err(first_err) => {
            let sanitized = sanitize_control_chars_in_strings(body);
            serde_json::from_str(&sanitized).map_err(|_| {
                OakError::Server(format!(
                    "could not parse the CI API response as JSON: {first_err}"
                ))
            })
        }
    }
}

/// Rewrite raw control characters (U+0000..U+001F) that appear *inside* JSON
/// string literals to their `\uXXXX` escapes, leaving structural whitespace
/// between tokens untouched. Tracks backslash escapes so an already-escaped
/// `\"` doesn't end the string early.
pub fn sanitize_control_chars_in_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in input.chars() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// API client
// ---------------------------------------------------------------------------

/// Everything needed to talk to one repo's CI API. Built from repo metadata
/// (`CiClient::from_repo`) or assembled directly by callers that already
/// resolved the remote (the merge --wait poll loop).
pub struct CiClient {
    pub remote: String,
    pub owner: String,
    pub repo: String,
    pub token: Option<String>,
}

impl CiClient {
    /// Ordinary CI only: capability discovery cannot fall back to a mutation
    /// whose exact-head/idempotency semantics have not been advertised.
    pub async fn trigger_exact(
        &self,
        branch: &str,
        expected_commit: &str,
        key: &str,
        workflow: Option<&str>,
    ) -> Result<TriggerReceipt> {
        if expected_commit.len() != 64
            || !expected_commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || key.is_empty()
            || key.len() > 128
            || !key.bytes().all(|b| b.is_ascii_graphic())
            || branch.is_empty()
            || branch.len() > 255
            || branch.chars().any(char::is_control)
            || workflow.is_some_and(|w| {
                w.is_empty() || w.len() > 255 || w.contains('/') || w.chars().any(char::is_control)
            })
        {
            return Err(OakError::InvalidArgument("Expected a full lowercase commit hash, a 1–128 non-space ASCII idempotency key, and valid branch/workflow names".into()));
        }
        let base = self.runs_url();
        let base = base.strip_suffix("/runs").expect("runs URL suffix");
        let http = crate::http::api_client();
        let response = self
            .authed(http.get(format!("{base}/capabilities")))
            .timeout(CI_MUTATION_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                OakError::Http(format!("CI capability request failed before dispatch: {e}"))
            })?;
        let status = response.status();
        let body = bounded_ci_body(response).await?;
        if !status.is_success() {
            return Err(OakError::Server(format!("ordinary_trigger_v1 is unavailable (HTTP {status}); no CI request was dispatched. Check server support and repository write access. The response body was omitted because remote diagnostics may contain secrets")));
        }
        let capabilities: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            OakError::Server("Invalid CI capability response; no CI request was dispatched".into())
        })?;
        if capabilities
            .get("ordinary_trigger_v1")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        {
            return Err(OakError::Server("Server does not advertise ordinary_trigger_v1; no CI request was dispatched. Upgrade the server before using oak ci trigger".into()));
        }
        let ambiguity = |error: OakError| {
            OakError::Server(format!("CI trigger outcome could not be confirmed: {error}. Retry the identical request with the same idempotency key; do not generate a new key"))
        };
        let response = self.authed(http.post(format!("{base}/trigger"))).timeout(CI_MUTATION_TIMEOUT).json(&serde_json::json!({
            "protocol":"ordinary_trigger_v1", "branch":branch,"expected_commit":expected_commit,"idempotency_key":key,"workflow":workflow
        })).send().await.map_err(|e| ambiguity(OakError::Http(e.to_string())))?;
        let status = response.status();
        let body = bounded_ci_body(response).await.map_err(ambiguity)?;
        if !status.is_success() {
            return Err(OakError::Server(format!("CI trigger rejected (HTTP {status}). A 412 requires reviewing the moved head; a 409 means the key belongs to a different request. For transient/uncertain outcomes retry the identical request with the same idempotency key. The response body was omitted because remote diagnostics may contain secrets")));
        }
        let receipt: TriggerReceipt = serde_json::from_str(&body).map_err(|_| {
            ambiguity(OakError::Server(
                "invalid receipt; parser diagnostics omitted because they may contain remote secrets"
                    .into(),
            ))
        })?;
        if receipt.protocol != "ordinary_trigger_v1"
            || receipt.branch != branch
            || receipt.commit_hash != expected_commit
            || !valid_run_ids(&receipt.run_ids)
        {
            return Err(ambiguity(OakError::Server(
                "CI receipt identity or run IDs do not match the request".into(),
            )));
        }
        Ok(receipt)
    }
    /// Resolve remote/owner/repo/token from the repo at `path`, using the
    /// same precedence as push/merge: `OAK_API_KEY` → repo `ApiKey` metadata
    /// → the stored login for the remote.
    pub fn from_repo(path: &Path) -> Result<Self> {
        let ctx = crate::resolve::resolve(path)?;
        let repo = ctx.open()?;
        Self::from_open_repo(repo.as_ref())
    }

    pub fn from_open_repo(repo: &dyn Repository) -> Result<Self> {
        let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
            OakError::Server(
                "Repository has no remote configured. Run `oak push` to link it to a server."
                    .to_string(),
            )
        })?;
        let (owner, name) = super::read_repo_identity(repo)?;
        let token = super::credentials::effective_token(
            &remote,
            repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
        );
        Ok(Self {
            remote,
            owner,
            repo: name,
            token,
        })
    }

    fn runs_url(&self) -> String {
        format!(
            "{}/api/{}/{}/ci/runs",
            self.remote.trim_end_matches('/'),
            self.owner,
            self.repo,
        )
    }

    fn run_url(&self, id: u64) -> Result<String> {
        let mut url = reqwest::Url::parse(&self.remote)
            .map_err(|_| OakError::Server("Invalid CI remote URL".into()))?;
        url.set_query(None);
        url.set_fragment(None);
        url.set_username("")
            .map_err(|_| OakError::Server("Invalid CI remote URL".into()))?;
        url.set_password(None)
            .map_err(|_| OakError::Server("Invalid CI remote URL".into()))?;
        url.path_segments_mut()
            .map_err(|_| OakError::Server("Invalid CI remote URL".into()))?
            .pop_if_empty()
            .push(&self.owner)
            .push(&self.repo)
            .push("ci")
            .push("runs")
            .push(&id.to_string());
        Ok(url.to_string())
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.header("authorization", format!("Bearer {t}")),
            None => req,
        }
    }

    /// `GET /ci/runs?limit=N` — recent runs, newest first.
    pub async fn list_runs(&self, limit: usize) -> Result<Vec<CiRun>> {
        let client = crate::http::api_client();
        let req = self
            .authed(client.get(self.runs_url()))
            .query(&[("limit", limit.to_string())]);
        let resp = req
            .send()
            .await
            .map_err(|e| OakError::Http(format!("CI runs request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(OakError::Server(format!(
                "could not list CI runs: {}",
                crate::http::error_text(resp).await
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        let mut runs: Vec<CiRun> = parse_json_lenient(&body)?;
        runs.truncate(limit);
        Ok(runs)
    }

    /// `GET /ci/runs/:id` — one run with jobs, steps, and step logs.
    pub async fn get_run(&self, id: u64) -> Result<CiRun> {
        let client = crate::http::api_client();
        let req = self.authed(client.get(format!("{}/{id}", self.runs_url())));
        let resp = req
            .send()
            .await
            .map_err(|e| OakError::Http(format!("CI run request failed: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(OakError::Server(format!(
                "CI run {id} not found — list recent runs with `oak ci runs`"
            )));
        }
        if !resp.status().is_success() {
            return Err(OakError::Server(format!(
                "could not fetch CI run {id}: {}",
                crate::http::error_text(resp).await
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        parse_json_lenient(&body)
    }

    async fn cancel_run_metadata(&self, id: u64) -> Result<CiRun> {
        let client = crate::http::api_client();
        let response = self
            .authed(client.get(format!("{}/{id}", self.runs_url())))
            .timeout(CI_MUTATION_TIMEOUT)
            .send()
            .await
            .map_err(|_| {
                OakError::Http(
                    "CI cancellation preflight had a network failure before any cancellation request; no cancellation request was sent"
                        .into(),
                )
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(OakError::Server(format!(
                "CI cancellation is unavailable: exact run {id} could not be read (HTTP 404). The server may not support CI or the run may not belong to this repository; no cancellation request was sent"
            )));
        }
        if status.is_redirection() {
            return Err(OakError::Server(format!(
                "CI cancellation preflight was redirected (HTTP {status}); no cancellation request was sent and the redirect target was not followed"
            )));
        }
        if !status.is_success() {
            return Err(OakError::Server(format!(
                "CI cancellation preflight failed (HTTP {status}); no cancellation request was sent. The response body was omitted because remote diagnostics may contain secrets"
            )));
        }
        let body = bounded_ci_body(response).await.map_err(|error| {
            let reason = if error.to_string().contains("1 MiB") {
                "the response exceeds the 1 MiB metadata budget"
            } else if error.to_string().contains("UTF-8") {
                "the response is not UTF-8"
            } else {
                "the bounded response stream failed"
            };
            OakError::Server(format!(
                "CI cancellation preflight could not safely read exact run {id}: {reason}; no cancellation request was sent. Inspect metadata read-only with `oak ci logs {id} --summary --json`"
            ))
        })?;
        parse_json_lenient(&body).map_err(|_| {
            OakError::Server(
                "CI cancellation preflight returned malformed run metadata; no cancellation request was sent. Parser diagnostics were omitted because they may contain remote secrets"
                    .into(),
            )
        })
    }

    /// Cancel one exact ordinary push/merge run through the legacy endpoint.
    /// Client preflight checks the immutable run id/commit/event record;
    /// repository authorization remains server-side. This is not an atomic
    /// branch-head comparison and does not advertise a new server capability.
    pub async fn cancel_exact(&self, run_id: u64, expected_commit: &str) -> Result<CancelReceipt> {
        if run_id == 0
            || run_id > i64::MAX as u64
            || expected_commit.len() != 64
            || !expected_commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(OakError::InvalidArgument(
                "CI cancellation requires a positive run id and full lowercase 64-character commit hash"
                    .into(),
            ));
        }

        let run = self.cancel_run_metadata(run_id).await?;
        if run.id != run_id || run.commit_hash != expected_commit {
            return Err(OakError::Server(
                "CI run identity or commit moved or does not match the requested cancellation; no cancellation request was sent"
                    .into(),
            ));
        }
        if !matches!(run.event.as_str(), "push" | "merge") {
            return Err(OakError::Server(format!(
                "CI run {run_id} is not an ordinary push/merge run; manual, protected, release, and unknown events cannot be cancelled by this command. No cancellation request was sent"
            )));
        }
        if run.status == "completed" {
            return Err(OakError::Server(format!(
                "CI run {run_id} is already terminal; no cancellation request was sent"
            )));
        }
        if !matches!(run.status.as_str(), "queued" | "running")
            || run.conclusion.is_some()
            || run.error.is_some()
            || run.finished_at.is_some()
        {
            return Err(OakError::Server(format!(
                "CI run {run_id} has unknown or inconsistent lifecycle metadata; no cancellation request was sent"
            )));
        }

        let client = crate::http::api_client();
        let response = self
            .authed(client.post(format!("{}/{run_id}/cancel", self.runs_url())))
            .timeout(CI_MUTATION_TIMEOUT)
            .send()
            .await
            .map_err(|_| {
                OakError::Server(format!(
                    "CI cancellation outcome is unconfirmed after a network failure. Do not retry blindly; inspect run {run_id} read-only"
                ))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(CancelReceipt {
                run_id,
                commit_hash: expected_commit.to_owned(),
                event: run.event,
                outcome: "control_plane_cancellation_recorded",
                control_plane_cancellation_recorded: true,
                execution_stop: "unconfirmed_best_effort",
            });
        }
        if status == reqwest::StatusCode::CONFLICT {
            // A bare 409 is not a terminal-state certificate across server
            // versions. Confirm a race only through one fresh, bounded read.
            return match self.cancel_run_metadata(run_id).await {
                Ok(observed)
                    if observed.id == run_id
                        && observed.commit_hash == expected_commit
                        && observed.event == run.event
                        && observed.status == "completed" =>
                {
                    Err(OakError::Server(format!(
                        "CI cancellation was not accepted because run {run_id} became terminal; terminal no-op confirmed by a read-only exact-run re-fetch"
                    )))
                }
                _ => Err(OakError::Server(format!(
                    "CI cancellation_not_confirmed: run {run_id} returned HTTP 409, and a bounded read-only re-fetch did not confirm a matching terminal run. No retry was attempted"
                ))),
            };
        }
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
            || status.is_redirection()
            || status.is_success()
        {
            return Err(OakError::Server(format!(
                "CI cancellation outcome is unconfirmed after HTTP {status}. Do not retry blindly; inspect run {run_id} read-only. The response body and redirect target were omitted"
            )));
        }
        Err(OakError::Server(format!(
            "CI cancellation was rejected (HTTP {status}); no cancellation was recorded by this request. The response body was omitted because remote diagnostics may contain secrets"
        )))
    }

    /// `POST /ci/runs` — dispatch a manual run of `workflow` at
    /// requested `branch`/`commit`. Legacy servers can ignore commit and use
    /// the branch head; an ID-only receipt is not proof of an exact commit.
    pub async fn dispatch_run(&self, workflow: &str, branch: &str, commit: &str) -> Result<CiRun> {
        Ok(self.dispatch_runs(workflow, branch, commit).await?.run)
    }

    /// Full legacy dispatch receipt, including every returned run ID.
    pub async fn dispatch_runs(
        &self,
        workflow: &str,
        branch: &str,
        commit: &str,
    ) -> Result<CiDispatchReceipt> {
        let client = crate::http::api_client();
        let req = self
            .authed(client.post(self.runs_url()))
            .timeout(CI_MUTATION_TIMEOUT)
            .json(&serde_json::json!({
                "workflow": workflow,
                "branch": branch,
                "commit": commit,
                "event": "manual",
            }));
        let resp = req
            .send()
            .await
            .map_err(|_| OakError::Http("CI rerun outcome is unconfirmed after a network failure; inspect oak ci runs before dispatching again. Transport diagnostics were omitted because they may contain credentials or private URLs".into()))?;
        let status = resp.status();
        if !status.is_success() {
            // A legacy error/redirect is not a receipt proving whether the
            // mutation happened. Do not read or echo arbitrary error bodies,
            // follow Location, or retry this non-idempotent dispatch.
            return Err(OakError::Server(format!(
                "CI rerun outcome is unconfirmed after HTTP {status}; inspect oak ci runs before dispatching again. The response body and redirect target were omitted"
            )));
        }
        let uncertain = |reason: &'static str| {
            OakError::Server(format!("CI rerun outcome is unconfirmed: {reason}; inspect oak ci runs before dispatching again. Remote parser diagnostics were omitted because they may contain secrets"))
        };
        let body = bounded_ci_body(resp)
            .await
            .map_err(|_| uncertain("the bounded 1 MiB receipt could not be read"))?;
        let value: serde_json::Value =
            parse_json_lenient(&body).map_err(|_| uncertain("malformed receipt JSON"))?;
        let run_ids = match value.get("run_ids") {
            Some(ids) => serde_json::from_value::<Vec<u64>>(ids.clone())
                .map_err(|_| uncertain("invalid run IDs"))?,
            None => vec![value
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| uncertain("no run IDs"))?],
        };
        if !valid_run_ids(&run_ids)
            || value
                .get("id")
                .is_some_and(|id| id.as_u64() != run_ids.first().copied())
        {
            return Err(uncertain(
                "empty, invalid, duplicate, or inconsistent run IDs",
            ));
        }
        let mut run: CiRun = serde_json::from_value(value.clone())
            .map_err(|_| uncertain("invalid run receipt fields"))?;
        // oak.space returns the ids created by a manual trigger as
        // `{ "run_ids": [...] }`, while older/self-hosted servers returned
        // `{ "id": ... }` (or a complete run). Preserve both wire shapes.
        // Keep the legacy `run` field as the first actual handle, but do not
        // discard additional IDs or fabricate requested branch/commit fields.
        run.id = run_ids[0];
        Ok(CiDispatchReceipt { run, run_ids })
    }

    /// The most recent run for `commit`, if any. The list endpoint has no
    /// commit filter, so this filters client-side over recent runs.
    pub async fn latest_run_for_commit(&self, commit: &str) -> Result<Option<CiRun>> {
        let runs = self.list_runs(STATUS_SCAN_LIMIT).await?;
        Ok(runs
            .into_iter()
            .filter(|r| r.commit_hash == commit)
            .max_by_key(|r| r.id))
    }
}

// ---------------------------------------------------------------------------
// JSON envelopes (append-only per AGENTS.md schema policy)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RunsJson<'a> {
    schema_version: u32,
    runs: &'a [CiRun],
}

#[derive(Serialize)]
struct StatusJson<'a> {
    schema_version: u32,
    branch: &'a str,
    commit: &'a str,
    /// "success" | "failure" | "running" | "no_runs"
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<&'a CiRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_url: Option<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct LogsJson<'a> {
    schema_version: u32,
    run: &'a CiRun,
    run_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_bytes_returned: Option<usize>,
}

#[derive(Serialize)]
struct RerunJson<'a> {
    schema_version: u32,
    rerun_of: u64,
    run: &'a CiRun,
    run_ids: &'a [u64],
    requested_branch: &'a str,
    requested_commit: &'a str,
    requested_commit_confirmed: bool,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct WaitObservation<'a> {
    requested_run_id: u64,
    observed_run_id: u64,
    observed_commit: &'a str,
    state: &'a str,
    run: &'a CiRun,
}

#[derive(Serialize)]
struct WaitJson<'a> {
    schema_version: u32,
    requested_run_ids: &'a [u64],
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_commit: Option<&'a str>,
    /// "success" | "failure" | "timeout"
    state: &'a str,
    observations: Vec<WaitObservation<'a>>,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct CancelJson<'a> {
    schema_version: u32,
    run_id: u64,
    commit_hash: &'a str,
    event: &'a str,
    outcome: &'a str,
    control_plane_cancellation_recorded: bool,
    execution_stop: &'a str,
    recommended_next_commands: Vec<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn trigger(
    path: &Path,
    branch: Option<&str>,
    expected_commit: &str,
    key: &str,
    workflow: Option<&str>,
    json: bool,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let api = CiClient::from_open_repo(repo.as_ref())?;
    let branch = match branch {
        Some(branch) => branch.to_owned(),
        None => repo.get_current_branch_name()?.ok_or_else(|| {
            OakError::InvalidArgument("No current branch; supply --branch".into())
        })?,
    };
    let receipt = api
        .trigger_exact(&branch, expected_commit, key, workflow)
        .await?;
    let ids = receipt
        .run_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let follow = format!("oak ci wait {ids} --commit {} --json", receipt.commit_hash);
    if json {
        let mut value =
            serde_json::to_value(&receipt).map_err(|e| OakError::Server(e.to_string()))?;
        value["schema_version"] = serde_json::json!(SCHEMA_VERSION);
        value["recommended_next_commands"] = serde_json::json!([follow]);
        output::print_json(&value)
    } else {
        output::success(&format!(
            "{} CI runs {ids} at {}@{}",
            if receipt.replayed {
                "Replayed receipt for"
            } else {
                "Created"
            },
            receipt.branch,
            receipt.commit_hash
        ));
        output::info(&follow);
        Ok(())
    }
}

/// `oak ci cancel RUN_ID --commit HASH [--json]` — cancel only an exact,
/// ordinary push/merge run. The server records control-plane cancellation;
/// stopping already-running backend execution remains best effort.
pub async fn cancel(path: &Path, run_id: u64, expected_commit: &str, json: bool) -> Result<()> {
    let api = CiClient::from_repo(path)?;
    let receipt = api.cancel_exact(run_id, expected_commit).await?;
    let follow = format!("oak ci status --run {run_id} --commit {expected_commit} --json");
    if json {
        output::print_json(&CancelJson {
            schema_version: SCHEMA_VERSION,
            run_id: receipt.run_id,
            commit_hash: &receipt.commit_hash,
            event: &receipt.event,
            outcome: receipt.outcome,
            control_plane_cancellation_recorded: receipt.control_plane_cancellation_recorded,
            execution_stop: receipt.execution_stop,
            recommended_next_commands: vec![follow],
        })
    } else {
        output::success(&format!(
            "Recorded control-plane cancellation for ordinary {} CI run #{run_id} at {expected_commit}.",
            receipt.event
        ));
        output::warning(
            "Backend execution stop is best effort and is not confirmed by this response.",
        );
        output::info(&follow);
        Ok(())
    }
}

/// `oak ci runs [--limit N] [--json]` — recent runs for this repo.
pub async fn runs(path: &Path, limit: usize, json: bool) -> Result<()> {
    let api = CiClient::from_repo(path)?;
    let runs = api.list_runs(limit.max(1)).await?;

    if json {
        return output::print_json(&RunsJson {
            schema_version: SCHEMA_VERSION,
            runs: &runs,
        });
    }

    if runs.is_empty() {
        output::info("No CI runs yet — a push or merge dispatches the repo's workflows.");
        return Ok(());
    }
    for run in &runs {
        output::print_line(&run.summary_line());
        if let Some(err) = run.error.as_deref().filter(|e| !e.trim().is_empty()) {
            output::print_line(&format!("       error: {err}"));
        }
    }
    Ok(())
}

/// `oak ci status [--json]` — the latest run for the current branch head
/// (the commit the server's merge gate checks). Returns the process exit
/// code: 0 concluded success, 1 concluded failure (or no runs), 3 still
/// running — distinct so scripts can branch without parsing.
pub async fn status(path: &Path, json: bool) -> Result<i32> {
    status_with_run(path, json, None, None).await
}

pub async fn status_with_run(
    path: &Path,
    json: bool,
    run_id: Option<u64>,
    expected_commit: Option<&str>,
) -> Result<i32> {
    if run_id == Some(0) || (expected_commit.is_some() && run_id.is_none()) {
        return Err(OakError::Server(
            "Exact CI status requires a positive run id".into(),
        ));
    }
    if let Some(hash) = expected_commit {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OakError::Server(
                "Expected CI commit must be a full 64-character hash".into(),
            ));
        }
    }
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let api = CiClient::from_open_repo(repo.as_ref())?;
    let (branch, head, run) = if let Some(id) = run_id {
        let mut run = api.get_run(id).await?;
        if run.id != id
            || expected_commit
                .is_some_and(|expected| !expected.eq_ignore_ascii_case(&run.commit_hash))
        {
            return Err(OakError::Server(
                "CI run identity or commit does not match the requested evidence".into(),
            ));
        }
        // Status is metadata-only even when the legacy detail endpoint includes logs.
        run.jobs = None;
        run.extra.clear();
        (run.branch.clone(), run.commit_hash.clone(), Some(run))
    } else {
        let branch = repo
            .get_current_branch_name()?
            .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
        let head = repo
            .get_branch_head(&branch)?
            .ok_or(OakError::NoCommits)?
            .to_string();
        let run = api.latest_run_for_commit(&head).await?;
        (branch, head, run)
    };
    let short = &head[..head.len().min(12)];

    let Some(run) = run else {
        if json {
            output::print_json(&StatusJson {
                schema_version: SCHEMA_VERSION,
                branch: &branch,
                commit: &head,
                state: "no_runs",
                run: None,
                run_url: None,
                recommended_next_commands: vec!["oak push".to_string(), "oak ci runs".to_string()],
            })?;
        } else {
            output::warning(&format!(
                "No CI runs found for {branch}@{short} — `oak push` dispatches CI; see recent runs with `oak ci runs`."
            ));
        }
        return Ok(CiGateState::Failure.exit_code());
    };

    let state = run.gate_state();
    if json {
        let recommended = match state {
            CiGateState::Success => vec!["oak merge".to_string()],
            CiGateState::Failure => vec![
                format!("oak ci logs {}", run.id),
                format!("oak ci rerun {}", run.id),
            ],
            CiGateState::Running => vec![
                format!("oak ci wait {} --commit {head} --json", run.id),
                format!("oak ci logs {}", run.id),
            ],
        };
        output::print_json(&StatusJson {
            schema_version: SCHEMA_VERSION,
            branch: &branch,
            commit: &head,
            state: state.as_str(),
            run: Some(&run),
            run_url: Some(api.run_url(run.id)?),
            recommended_next_commands: recommended,
        })?;
        return Ok(state.exit_code());
    }

    output::print_line(&run.summary_line());
    match state {
        CiGateState::Success => {
            output::success(&format!(
                "CI passed for {branch}@{short} — `oak merge` is unblocked."
            ));
        }
        CiGateState::Failure => {
            if let Some(err) = run.error.as_deref().filter(|e| !e.trim().is_empty()) {
                output::print_line(&format!("       error: {err}"));
            }
            output::error(&format!(
                "CI failed for {branch}@{short} — inspect with `oak ci logs {}`, re-run with `oak ci rerun {}`.",
                run.id, run.id
            ));
        }
        CiGateState::Running => {
            output::info(&format!(
                "CI is still running for {branch}@{short} — wait read-only with `oak ci wait {} --commit {head}`.", run.id
            ));
        }
    }
    Ok(state.exit_code())
}

/// `oak ci logs <run-id> [--json]` — a run's step-by-step logs.
pub async fn logs(path: &Path, run_id: u64, json: bool) -> Result<()> {
    logs_with_options(path, run_id, json, LogOptions::default()).await
}

/// Optional projections preserve the legacy full-log default. The budget
/// covers returned log text, not metadata or HTTP transfer on older servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogOptions {
    pub summary: bool,
    pub failed: bool,
    pub max_bytes: Option<usize>,
}

pub async fn logs_with_options(
    path: &Path,
    run_id: u64,
    json: bool,
    options: LogOptions,
) -> Result<()> {
    let api = CiClient::from_repo(path)?;
    let mut run = api.get_run(run_id).await?;
    if run.id != run_id {
        return Err(OakError::Server(format!(
            "Requested CI run {run_id}, received {}",
            run.id
        )));
    }
    let projected = options.summary || options.failed || options.max_bytes.is_some();
    let mut remaining =
        options
            .max_bytes
            .unwrap_or(if options.failed { 65_536 } else { usize::MAX });
    let mut returned = 0;
    let mut truncated = false;
    if projected {
        // Unknown fields may themselves contain logs/scripts; opt-in compact
        // projections whitelist known metadata rather than copying those fields.
        run.extra.clear();
        if let Some(jobs) = &mut run.jobs {
            for job in jobs.iter_mut() {
                job.extra.clear();
                if options.failed {
                    job.steps.retain(|step| {
                        step.exit_code.is_some_and(|code| code != 0)
                            || matches!(
                                step.conclusion.as_deref(),
                                Some("failure" | "timed_out" | "cancelled" | "action_required")
                            )
                            || matches!(
                                step.status.as_str(),
                                "failed" | "failure" | "timed_out" | "cancelled"
                            )
                    });
                }
                for step in &mut job.steps {
                    step.extra.clear();
                    step.command = None;
                    if options.summary {
                        step.logs = None;
                    } else if let Some(log) = &mut step.logs {
                        let mut end = log.len().min(remaining);
                        while !log.is_char_boundary(end) {
                            end -= 1;
                        }
                        truncated |= end < log.len();
                        log.truncate(end);
                        remaining -= end;
                        returned += end;
                    }
                }
            }
            if options.failed {
                jobs.retain(|job| {
                    !job.steps.is_empty() || job.conclusion.as_deref() == Some("failure")
                });
            }
        }
    }
    let run_url = api.run_url(run_id)?;

    if json {
        return output::print_json(&LogsJson {
            schema_version: SCHEMA_VERSION,
            run: &run,
            run_url: run_url.to_string(),
            projection: projected.then_some(if options.summary {
                "summary"
            } else if options.failed {
                "failed"
            } else {
                "bounded"
            }),
            logs_truncated: projected.then_some(truncated),
            log_bytes_returned: projected.then_some(returned),
        });
    }

    output::print_line(&run.summary_line());
    if let Some(err) = run.error.as_deref().filter(|e| !e.trim().is_empty()) {
        output::print_line(&format!("       error: {err}"));
    }
    let Some(jobs) = &run.jobs else {
        output::info("No job details available for this run.");
        return Ok(());
    };
    for job in jobs {
        for step in &job.steps {
            let outcome = step
                .conclusion
                .clone()
                .unwrap_or_else(|| step.status.clone());
            let exit = step
                .exit_code
                .map(|c| format!(", exit {c}"))
                .unwrap_or_default();
            output::print_line(&format!(
                "── {} / {}  ({outcome}{exit})",
                job.name, step.name
            ));
            if let Some(logs) = step.logs.as_deref().filter(|l| !l.is_empty()) {
                output::print_line(logs.trim_end_matches('\n'));
            }
        }
    }
    if truncated {
        output::warning("CI logs truncated to the requested byte budget; use --max-bytes with a larger value or inspect the run URL.");
        output::print_line(run_url.as_str());
    }
    Ok(())
}

/// `oak ci rerun <run-id> [--json]` — re-dispatch a run's workflow at the
/// requested branch/commit (event `manual`) and report all actual run IDs.
/// Legacy servers may use the current branch head instead of that commit.
pub async fn rerun(path: &Path, run_id: u64, json: bool) -> Result<()> {
    let api = CiClient::from_repo(path)?;
    let old = api.get_run(run_id).await?;
    if run_id == 0
        || old.id != run_id
        || old.workflow_name.is_empty()
        || old.branch.is_empty()
        || old.commit_hash.is_empty()
    {
        return Err(OakError::Server(
            "CI source run identity/workflow is missing or mismatched; no rerun dispatched".into(),
        ));
    }
    let receipt = api
        .dispatch_runs(&old.workflow_name, &old.branch, &old.commit_hash)
        .await?;
    let new_run = &receipt.run;
    let requested_commit_confirmed = receipt.run_ids.len() == 1
        && new_run.branch == old.branch
        && new_run.commit_hash == old.commit_hash;
    let mut recommended_next_commands = vec!["oak ci status".to_string()];
    recommended_next_commands.extend(receipt.run_ids.iter().map(|id| format!("oak ci logs {id}")));

    if json {
        return output::print_json(&RerunJson {
            schema_version: SCHEMA_VERSION,
            rerun_of: run_id,
            run: new_run,
            run_ids: &receipt.run_ids,
            requested_branch: &old.branch,
            requested_commit: &old.commit_hash,
            requested_commit_confirmed,
            recommended_next_commands,
        });
    }

    output::success(&format!(
        "Dispatched runs {} (requested re-run of #{run_id}, workflow '{}')",
        receipt
            .run_ids
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", "),
        old.workflow_name
    ));
    if !requested_commit_confirmed {
        output::warning("The legacy server receipt does not confirm the requested commit; it may have used the current branch head. Inspect the returned runs before relying on them.");
    }
    for command in recommended_next_commands {
        output::info(&command);
    }
    Ok(())
}

/// Wait for one or more exact run ids to reach a terminal state. A supplied
/// commit is an additional fence: observing any run at a different commit
/// fails closed before the command reports a gate result.
pub async fn wait(
    path: &Path,
    requested_run_ids: &[u64],
    expected_commit: Option<&str>,
    timeout: std::time::Duration,
    json: bool,
) -> Result<i32> {
    if requested_run_ids.is_empty() {
        return Err(OakError::InvalidArgument(
            "oak ci wait requires at least one run id".to_string(),
        ));
    }
    if requested_run_ids.contains(&0) {
        return Err(OakError::InvalidArgument(
            "oak ci wait requires non-zero run ids".to_string(),
        ));
    }

    let api = CiClient::from_repo(path)?;
    let started = tokio::time::Instant::now();
    let poll_once = timeout.is_zero();
    // A zero timeout is the documented one-shot probe. Give that single
    // request a small, finite I/O budget so an immediately available result
    // can still be observed without allowing a stalled peer to hang forever.
    let deadline = started
        + if poll_once {
            std::time::Duration::from_secs(1)
        } else {
            timeout
        };
    let mut observed = std::collections::BTreeMap::<u64, CiRun>::new();

    loop {
        let mut request_timed_out = false;
        for requested in requested_run_ids {
            if observed
                .get(requested)
                .is_some_and(|run| run.gate_state() != CiGateState::Running)
            {
                continue;
            }
            let run = match tokio::time::timeout_at(deadline, api.get_run(*requested)).await {
                Ok(result) => result?,
                Err(_) => {
                    request_timed_out = true;
                    break;
                }
            };
            if run.id != *requested {
                return Err(OakError::Server(format!(
                    "CI wait requested run #{requested}, but the server returned run #{}; refusing to follow a different run",
                    run.id
                )));
            }
            if let Some(expected) = expected_commit {
                if run.commit_hash != expected {
                    return Err(OakError::Server(format!(
                        "CI wait requested commit {expected} for run #{requested}, but the server reported commit {}; refusing a stale or unrelated result",
                        run.commit_hash
                    )));
                }
            }
            observed.insert(*requested, run);
        }

        let any_failure = observed
            .values()
            .any(|run| run.gate_state() == CiGateState::Failure);
        let all_terminal = requested_run_ids.iter().all(|id| {
            observed
                .get(id)
                .is_some_and(|run| run.gate_state() != CiGateState::Running)
        });
        let timed_out = !all_terminal
            && (request_timed_out || poll_once || tokio::time::Instant::now() >= deadline);
        if all_terminal || timed_out {
            let state = if timed_out {
                "timeout"
            } else if any_failure {
                "failure"
            } else {
                "success"
            };
            let exit = if timed_out {
                CiGateState::Running.exit_code()
            } else if any_failure {
                CiGateState::Failure.exit_code()
            } else {
                CiGateState::Success.exit_code()
            };
            let observations = requested_run_ids
                .iter()
                .filter_map(|requested| {
                    observed.get(requested).map(|run| WaitObservation {
                        requested_run_id: *requested,
                        observed_run_id: run.id,
                        observed_commit: &run.commit_hash,
                        state: run.gate_state().as_str(),
                        run,
                    })
                })
                .collect();
            let recommended_next_commands = match state {
                "success" => vec!["oak merge".to_string()],
                "failure" => observed
                    .values()
                    .filter(|run| run.gate_state() == CiGateState::Failure)
                    .map(|run| format!("oak ci logs {}", run.id))
                    .collect(),
                _ => {
                    let mut command = format!(
                        "oak ci wait {}",
                        requested_run_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    if let Some(commit) = expected_commit {
                        command.push_str(&format!(" --commit {commit}"));
                    }
                    if json {
                        command.push_str(" --json");
                    }
                    vec![command]
                }
            };

            if json {
                output::print_json(&WaitJson {
                    schema_version: SCHEMA_VERSION,
                    requested_run_ids,
                    expected_commit,
                    state,
                    observations,
                    recommended_next_commands,
                })?;
            } else {
                for run in observed.values() {
                    output::print_line(&run.summary_line());
                }
                match state {
                    "success" => output::success("All requested CI runs passed."),
                    "failure" => output::error("One or more requested CI runs failed."),
                    _ => output::warning("Timed out while requested CI runs were still running."),
                }
            }
            return Ok(exit);
        }

        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(1)),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_escapes_raw_control_chars_inside_strings_only() {
        // Raw ESC + NUL + newline inside the string value; structural
        // whitespace (the pretty-printing newline/indent) untouched.
        let raw = "{\n  \"logs\": \"a\u{1b}[1mb\u{0}c\nd\"\n}";
        let sane = sanitize_control_chars_in_strings(raw);
        assert_eq!(sane, "{\n  \"logs\": \"a\\u001b[1mb\\u0000c\\u000ad\"\n}");
        let v: serde_json::Value = serde_json::from_str(&sane).unwrap();
        assert_eq!(v["logs"].as_str().unwrap(), "a\u{1b}[1mb\u{0}c\nd");
    }

    #[test]
    fn sanitize_respects_escaped_quotes_and_backslashes() {
        // `\"` must not end the string; `\\` must not arm an escape for the
        // following quote.
        let raw = "{\"a\": \"x\\\"\u{1b}y\\\\\", \"b\": 1}";
        let sane = sanitize_control_chars_in_strings(raw);
        let v: serde_json::Value = serde_json::from_str(&sane).unwrap();
        assert_eq!(v["a"].as_str().unwrap(), "x\"\u{1b}y\\");
        assert_eq!(v["b"], 1);
    }

    #[test]
    fn lenient_parse_recovers_run_with_raw_control_chars_in_logs() {
        let body = "{\"id\":153,\"workflow_name\":\"ci\",\"status\":\"completed\",\
                    \"conclusion\":\"failure\",\"error\":\"sandbox died (worker redeploy or eviction)\",\
                    \"jobs\":[{\"id\":1,\"name\":\"check\",\"status\":\"completed\",\"conclusion\":\"failure\",\
                    \"steps\":[{\"id\":9,\"name\":\"test\",\"status\":\"completed\",\"conclusion\":\"failure\",\
                    \"exit_code\":101,\"logs\":\"\u{1b}[31merror\u{1b}[0m\nboom\"}]}]}";
        // Strict serde_json must refuse this (raw ESC / newline in a string)…
        assert!(serde_json::from_str::<CiRun>(body).is_err());
        // …but the lenient path parses it.
        let run: CiRun = parse_json_lenient(body).unwrap();
        assert_eq!(run.id, 153);
        assert_eq!(run.gate_state(), CiGateState::Failure);
        let logs = run.jobs.as_ref().unwrap()[0].steps[0]
            .logs
            .as_deref()
            .unwrap();
        assert!(logs.contains("\u{1b}[31merror"));
        assert!(logs.contains("boom"));
    }

    #[test]
    fn lenient_parse_reports_first_error_when_body_is_hopeless() {
        let err = parse_json_lenient::<CiRun>("<html>502 bad gateway</html>").unwrap_err();
        assert!(matches!(err, OakError::Server(_)), "got {err:?}");
    }

    #[test]
    fn unknown_fields_survive_the_json_round_trip() {
        let body = r#"{"id":7,"workflow_name":"ci","status":"completed","conclusion":"success","novel_field":"kept"}"#;
        let run: CiRun = parse_json_lenient(body).unwrap();
        let out = serde_json::to_value(&run).unwrap();
        assert_eq!(out["novel_field"], "kept");
    }

    #[test]
    fn gate_state_maps_status_and_conclusion() {
        let mut run = CiRun {
            status: "running".to_string(),
            ..CiRun::default()
        };
        assert_eq!(run.gate_state(), CiGateState::Running);
        run.status = "queued".to_string();
        assert_eq!(run.gate_state(), CiGateState::Running);
        run.conclusion = Some("success".to_string());
        assert_eq!(run.gate_state(), CiGateState::Success);
        run.conclusion = Some("failure".to_string());
        assert_eq!(run.gate_state(), CiGateState::Failure);
        run.conclusion = Some("cancelled".to_string());
        assert_eq!(run.gate_state(), CiGateState::Failure);
        // Completed with no conclusion at all = failure, not running.
        run.conclusion = None;
        run.status = "completed".to_string();
        assert_eq!(run.gate_state(), CiGateState::Failure);
        // Infra error while status never completed = failure too.
        run.status = "running".to_string();
        run.error = Some("sandbox died".to_string());
        assert_eq!(run.gate_state(), CiGateState::Failure);
    }

    #[test]
    fn exit_codes_are_distinct_per_state() {
        assert_eq!(CiGateState::Success.exit_code(), 0);
        assert_eq!(CiGateState::Failure.exit_code(), 1);
        assert_eq!(CiGateState::Running.exit_code(), 3);
    }

    #[test]
    fn duration_formats_compactly() {
        assert_eq!(format_duration_secs(45), "45s");
        assert_eq!(format_duration_secs(382), "6m22s");
        assert_eq!(format_duration_secs(3725), "1h02m");
        assert_eq!(format_duration_secs(-1), "-");
    }
}
