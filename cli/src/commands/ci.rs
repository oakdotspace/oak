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
/// Also the `--limit` advised after `no_runs`, so the follow-up inspects at
/// least as many runs as `status` already scanned.
pub const STATUS_SCAN_LIMIT: usize = 50;

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

/// A classified exact-cancel failure. `unknown` means the cancellation POST
/// may have been recorded (network loss, 408/5xx/redirect/unexpected success,
/// or an unconfirmed 409); otherwise this request cancelled nothing.
#[derive(Debug)]
struct CancelFailure {
    unknown: bool,
    error: OakError,
}

impl CancelFailure {
    fn refused(error: OakError) -> Self {
        Self {
            unknown: false,
            error,
        }
    }

    fn unknown(error: OakError) -> Self {
        Self {
            unknown: true,
            error,
        }
    }
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
        let token = super::credentials::effective_token_for_repository(&remote, repo);
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

    pub(crate) fn run_url(&self, id: u64) -> Result<String> {
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
        self.get_run_with_redaction(id, false).await
    }

    async fn get_run_with_redaction(&self, id: u64, redact: bool) -> Result<CiRun> {
        let client = crate::http::api_client();
        let req = self.authed(client.get(format!("{}/{id}", self.runs_url())));
        let resp = req.send().await.map_err(|e| {
            if redact {
                OakError::Http("CI run request failed; remote details omitted".into())
            } else {
                OakError::Http(format!("CI run request failed: {e}"))
            }
        })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(OakError::Server(format!(
                "CI run {id} not found — list recent runs with `oak ci runs`"
            )));
        }
        if !resp.status().is_success() {
            if redact {
                return Err(OakError::Server(format!(
                    "could not fetch CI run {id}: HTTP {}; remote details omitted",
                    resp.status().as_u16()
                )));
            }
            return Err(OakError::Server(format!(
                "could not fetch CI run {id}: {}",
                crate::http::error_text(resp).await
            )));
        }
        let body = resp.text().await.map_err(|e| {
            if redact {
                OakError::Http("CI run body could not be read; remote details omitted".into())
            } else {
                OakError::Http(e.to_string())
            }
        })?;
        parse_json_lenient(&body).map_err(|error| {
            if redact {
                OakError::Server("CI run response was invalid; parser details omitted".into())
            } else {
                error
            }
        })
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
        self.cancel_exact_classified(run_id, expected_commit)
            .await
            .map_err(|failure| failure.error)
    }

    /// [`Self::cancel_exact`] with the failure classified: a known refusal
    /// (nothing was cancelled by this request) versus an unknown outcome
    /// (the POST may or may not have been recorded).
    async fn cancel_exact_classified(
        &self,
        run_id: u64,
        expected_commit: &str,
    ) -> std::result::Result<CancelReceipt, CancelFailure> {
        if run_id == 0
            || run_id > i64::MAX as u64
            || expected_commit.len() != 64
            || !expected_commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CancelFailure::refused(OakError::InvalidArgument(
                "CI cancellation requires a positive run id and full lowercase 64-character commit hash"
                    .into(),
            )));
        }

        let run = self
            .cancel_run_metadata(run_id)
            .await
            .map_err(CancelFailure::refused)?;
        if run.id != run_id || run.commit_hash != expected_commit {
            return Err(CancelFailure::refused(OakError::Server(
                "CI run identity or commit moved or does not match the requested cancellation; no cancellation request was sent"
                    .into(),
            )));
        }
        if !matches!(run.event.as_str(), "push" | "merge") {
            return Err(CancelFailure::refused(OakError::Server(format!(
                "CI run {run_id} is not an ordinary push/merge run; manual, protected, release, and unknown events cannot be cancelled by this command. No cancellation request was sent"
            ))));
        }
        if run.status == "completed" {
            return Err(CancelFailure::refused(OakError::Server(format!(
                "CI run {run_id} is already terminal; no cancellation request was sent"
            ))));
        }
        if !matches!(run.status.as_str(), "queued" | "running")
            || run.conclusion.is_some()
            || run.error.is_some()
            || run.finished_at.is_some()
        {
            return Err(CancelFailure::refused(OakError::Server(format!(
                "CI run {run_id} has unknown or inconsistent lifecycle metadata; no cancellation request was sent"
            ))));
        }

        let client = crate::http::api_client();
        let response = self
            .authed(client.post(format!("{}/{run_id}/cancel", self.runs_url())))
            .timeout(CI_MUTATION_TIMEOUT)
            .send()
            .await
            .map_err(|_| {
                CancelFailure::unknown(OakError::Server(format!(
                    "CI cancellation outcome is unconfirmed after a network failure. Do not retry blindly; inspect run {run_id} read-only"
                )))
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
                    Err(CancelFailure::refused(OakError::Server(format!(
                        "CI cancellation was not accepted because run {run_id} became terminal; terminal no-op confirmed by a read-only exact-run re-fetch"
                    ))))
                }
                _ => Err(CancelFailure::unknown(OakError::Server(format!(
                    "CI cancellation_not_confirmed: run {run_id} returned HTTP 409, and a bounded read-only re-fetch did not confirm a matching terminal run. No retry was attempted"
                )))),
            };
        }
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
            || status.is_redirection()
            || status.is_success()
        {
            return Err(CancelFailure::unknown(OakError::Server(format!(
                "CI cancellation outcome is unconfirmed after HTTP {status}. Do not retry blindly; inspect run {run_id} read-only. The response body and redirect target were omitted"
            ))));
        }
        Err(CancelFailure::refused(OakError::Server(format!(
            "CI cancellation was rejected (HTTP {status}); no cancellation was recorded by this request. The response body was omitted because remote diagnostics may contain secrets"
        ))))
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
    /// What `branch`/`commit` describe: "current_head" (the checkout head the
    /// merge gate checks) or "exact_run" (the `--run` argument, whose subject
    /// may be another branch or an older head).
    subject: &'a str,
    /// The current checkout's branch and head, when both resolve locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_head: Option<&'a str>,
    /// True only when `branch`/`commit` equal the checkout's branch and head:
    /// the subject a bare `oak merge` would act on. A successful run whose
    /// subject differs is an observation about that subject, not merge advice
    /// for the current checkout.
    checkout_matches_observed: bool,
    recommended_next_commands: Vec<String>,
}

/// The current checkout's branch and head. Errors name the missing piece
/// (no current branch / no commits) so current-head status keeps its
/// existing failure contract.
fn checkout_identity(repo: &dyn Repository) -> Result<(String, String)> {
    let branch = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    let head = repo
        .get_branch_head(&branch)?
        .ok_or(OakError::NoCommits)?
        .to_string();
    Ok((branch, head))
}

/// Fresh checkout identity read from disk right now. `None` means the
/// identity is not positively established (no branch, no commits, or the
/// repository could not be reopened), so advice must not assume a bare
/// `oak merge` acts on the observed subject; it renders as `oak status`. Advice that follows a long wait
/// must call this at decision time, not reuse a snapshot from before the
/// wait: another process may have committed or switched meanwhile.
fn current_checkout_identity(path: &Path) -> Option<(String, String)> {
    let ctx = crate::resolve::resolve(path).ok()?;
    let repo = ctx.open().ok()?;
    checkout_identity(repo.as_ref()).ok()
}

/// One read-only `oak ci wait` invocation over exact ids. Every recommended
/// wait is built here so flag rules stay in one place. A `--commit` fence is
/// emitted only for a full 64-hex hash: a server-reported or user-supplied
/// value of any other shape is never interpolated into an advised command
/// (a real fence can only ever equal a full hash anyway).
fn wait_command(
    run_ids: &[u64],
    commit: Option<&str>,
    json: bool,
    summary: bool,
    timeout: Option<u64>,
) -> String {
    let mut command = format!(
        "oak ci wait {}",
        run_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );
    if let Some(commit) = commit.filter(|commit| summary_commit_valid(commit)) {
        command.push_str(&format!(" --commit {commit}"));
    }
    if json {
        command.push_str(" --json");
    }
    if summary {
        command.push_str(" --summary");
    }
    if let Some(timeout) = timeout {
        command.push_str(&format!(" --timeout {timeout}"));
    }
    command
}

/// Whether an observed `(branch, commit)` is exactly the checkout subject.
fn observed_is_checkout(checkout: Option<&(String, String)>, branch: &str, commit: &str) -> bool {
    checkout.is_some_and(|(checkout_branch, checkout_head)| {
        checkout_branch == branch && checkout_head.eq_ignore_ascii_case(commit)
    })
}

/// Next actions after every observed run in `observed` concluded success.
///
/// Bare `oak merge` acts on the current checkout, so it is recommended only
/// when every observed subject is exactly the checkout's branch and head.
/// Otherwise the advice stays read-only and bound to what was observed: the
/// checkout's own gate for an older head of the same branch, the remote
/// branch record for another branch, or the checkout identity when it could
/// not be established. CI success is never a merge prediction or
/// authorization; the server gate still decides.
fn success_advice(
    checkout: Option<&(String, String)>,
    observed: &[(&str, &str)],
    json: bool,
) -> Vec<String> {
    if !observed.is_empty()
        && observed
            .iter()
            .all(|(branch, commit)| observed_is_checkout(checkout, branch, commit))
    {
        return vec!["oak merge".to_string()];
    }
    let json_flag = if json { " --json" } else { "" };
    let Some((checkout_branch, _)) = checkout else {
        return vec![format!("oak status{json_flag}")];
    };
    let mut commands = Vec::new();
    for (branch, _) in observed {
        let command = if branch.is_empty() {
            // A run without a branch cannot be bound to any subject.
            format!("oak status{json_flag}")
        } else if *branch == checkout_branch {
            format!("oak ci status{json_flag}")
        } else {
            // `oak branch show --remote` requires --json on this binary, and
            // the server-reported name is shell-quoted by the shared builder
            // rather than interpolated raw.
            super::push::publication_branch_reconciliation_command(branch)
        };
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
}

/// Read-only follow-ups for a rerun receipt over the exact returned ids.
/// A confirmed receipt (one run whose server-reported branch/commit match the
/// request, with a full hash) gets a commit-fenced wait, in summary mode for
/// JSON because every observed source hash is then known to be full-length.
/// An unconfirmed receipt gets a one-shot probe first (immediate inspection
/// of what actually ran, never blocking) and then a plain wait: summary mode
/// would fail closed on legacy runs that omit the hash. Never branch-latest
/// status, eager logs, or another dispatch.
fn rerun_wait_advice(
    run_ids: &[u64],
    requested_commit: &str,
    requested_commit_confirmed: bool,
    json: bool,
) -> Vec<String> {
    let confirmed_full_hash = requested_commit_confirmed && summary_commit_valid(requested_commit);
    if confirmed_full_hash {
        return vec![wait_command(
            run_ids,
            Some(requested_commit),
            json,
            json,
            None,
        )];
    }
    let commit = requested_commit_confirmed.then_some(requested_commit);
    vec![
        wait_command(run_ids, commit, json, false, Some(0)),
        wait_command(run_ids, commit, json, false, None),
    ]
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
    /// Canonical web page for the run (derived from the configured remote).
    run_url: String,
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
    /// Present only for `--current`: how the head was resolved and bound.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    current: Option<CurrentTargetJson>,
    recommended_next_commands: Vec<String>,
}

/// Summary output never serializes the open-ended CiRun payload. All strings
/// here are fixed vocabulary, validated hashes, timestamps, or generated commands.
/// Collections from the server are bounded independently of job/log volume.
#[derive(Serialize)]
struct WaitSummaryObservation<'a> {
    requested_run_id: u64,
    observed_run_id: u64,
    observed_commit: &'a str,
    observed_at: &'a str,
    state: &'static str,
    failure_details: &'static str,
    run_error_present: bool,
    failed_jobs_total: usize,
    failed_job_ids: Vec<u64>,
    failed_jobs_omitted: usize,
    failed_steps_total: usize,
    failed_steps: Vec<WaitFailedStep>,
    failed_steps_omitted: usize,
    log_bytes_returned: usize,
    details_command: String,
    /// Canonical web page for the run, derived locally from the configured
    /// remote (credentials, query, and fragment stripped); never a
    /// server-supplied string.
    run_url: String,
}

#[derive(Serialize)]
struct WaitFailedStep {
    job_id: u64,
    step_id: u64,
    exit_code: Option<i64>,
}

#[derive(Serialize)]
struct WaitSummaryJson<'a> {
    schema_version: u32,
    projection: &'static str,
    provider: &'static str,
    execution_backend: &'static str,
    transfer_scope: &'static str,
    requested_run_ids: &'a [u64],
    expected_commit: Option<&'a str>,
    state: &'a str,
    observations: Vec<WaitSummaryObservation<'a>>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    current: Option<CurrentTargetJson>,
    recommended_next_commands: Vec<String>,
}

fn summary_failure(status: &str, conclusion: Option<&str>) -> bool {
    // Hosted CI treats conditional/skipped jobs and steps as non-failing.
    // This detail projection must use that contract without changing the run gate.
    conclusion.is_some_and(|c| !matches!(c, "success" | "skipped"))
        || matches!(status, "failed" | "failure" | "cancelled" | "timed_out")
        || (status == "completed" && conclusion.is_none())
}

fn wait_summary_observation<'a>(
    run: &'a CiRun,
    at: &'a str,
    run_url: String,
) -> WaitSummaryObservation<'a> {
    const DETAIL_LIMIT: usize = 8;
    let mut failed_job_ids = Vec::new();
    let mut failed_steps = Vec::new();
    let mut failed_jobs_total = 0;
    let mut failed_steps_total = 0;
    for job in run.jobs.iter().flatten() {
        if summary_failure(&job.status, job.conclusion.as_deref()) {
            failed_jobs_total += 1;
            if failed_job_ids.len() < DETAIL_LIMIT {
                failed_job_ids.push(job.id);
            }
        }
        for step in &job.steps {
            if summary_failure(&step.status, step.conclusion.as_deref())
                || step.exit_code.is_some_and(|code| code != 0)
            {
                failed_steps_total += 1;
                if failed_steps.len() < DETAIL_LIMIT {
                    failed_steps.push(WaitFailedStep {
                        job_id: job.id,
                        step_id: step.id,
                        exit_code: step.exit_code,
                    });
                }
            }
        }
    }
    WaitSummaryObservation {
        requested_run_id: run.id,
        observed_run_id: run.id,
        observed_commit: &run.commit_hash,
        observed_at: at,
        state: run.gate_state().as_str(),
        failure_details: if failed_jobs_total > 0 || failed_steps_total > 0 {
            "reported"
        } else if run.gate_state() == CiGateState::Failure {
            "unavailable"
        } else {
            "no_failures_reported"
        },
        run_error_present: run.error.is_some(),
        failed_jobs_omitted: failed_jobs_total - failed_job_ids.len(),
        failed_job_ids,
        failed_jobs_total,
        failed_steps_omitted: failed_steps_total - failed_steps.len(),
        failed_steps,
        failed_steps_total,
        log_bytes_returned: 0,
        details_command: format!("oak ci logs {} --summary --json", run.id),
        run_url,
    }
}

fn summary_commit_valid(commit: &str) -> bool {
    commit.len() == 64 && commit.bytes().all(|b| b.is_ascii_hexdigit())
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
    let follow = wait_command(
        &receipt.run_ids,
        Some(&receipt.commit_hash),
        true,
        false,
        None,
    );
    if json {
        let mut value =
            serde_json::to_value(&receipt).map_err(|e| OakError::Server(e.to_string()))?;
        value["schema_version"] = serde_json::json!(SCHEMA_VERSION);
        value["recommended_next_commands"] = serde_json::json!([follow]);
        output::print_json(&value)
    } else {
        let ids = receipt
            .run_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
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

/// How many recent runs `cancel --superseded` scans for the current branch.
pub const SUPERSEDED_SCAN_LIMIT: usize = 100;

#[derive(Serialize)]
struct SupersededCandidateJson {
    run_id: u64,
    commit_hash: String,
    event: String,
    status: String,
    workflow_name: String,
    run_url: String,
    /// Whether this run passed every client-side eligibility check.
    eligible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_reason: Option<&'static str>,
    /// "would_cancel" (dry run) | "cancelled" | "not_cancelled" (refused
    /// before or by the server; nothing cancelled by this request) |
    /// "outcome_unknown" (the POST may have been recorded: inspect read-only,
    /// never retry blindly) | "skipped" (ineligible; nothing sent)
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_stop: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct SupersededJson<'a> {
    schema_version: u32,
    mode: &'static str,
    dry_run: bool,
    branch: &'a str,
    head: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_branch_head: Option<&'a str>,
    /// Runs observed for exactly the current head on this branch.
    head_run_ids: Vec<u64>,
    runs_scanned: usize,
    scan_limit: usize,
    /// Why no cancellation may be sent at all (present only when blocked).
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_reason: Option<&'static str>,
    candidates: Vec<SupersededCandidateJson>,
    recommended_next_commands: Vec<String>,
}

/// `oak ci cancel --superseded [--yes] [--json]` (fb-358, client half).
///
/// Lists in-flight ordinary `push` runs on the *current branch* whose commit
/// is not the current head and which are older than the newest run for that
/// head. Dry run by default; `--yes` cancels each through the existing
/// exact-run path ([`CiClient::cancel_exact`]), which re-reads the run and
/// rejects manual/protected/release/terminal/moved runs before any POST.
///
/// Fails closed (sends nothing) unless the checkout head is exactly the
/// published remote branch head and a run for that head is observed: a stale
/// checkout must never cancel runs for a newer published commit.
/// Returns the exit code: 0 ok, 1 blocked under `--yes` or any cancellation
/// not confirmed.
pub async fn cancel_superseded(path: &Path, execute: bool, json: bool) -> Result<i32> {
    let (api, branch, head) = {
        let ctx = crate::resolve::resolve(path)?;
        let repo = ctx.open()?;
        let api = CiClient::from_open_repo(repo.as_ref())?;
        let (branch, head) = checkout_identity(repo.as_ref())?;
        (api, branch, head)
    };
    let remote = remote_branch_head(&api, &branch, CI_MUTATION_TIMEOUT).await;
    let runs = api.list_runs(SUPERSEDED_SCAN_LIMIT).await?;
    let mut head_run_ids: Vec<u64> = runs
        .iter()
        .filter(|run| run.branch == branch && run.commit_hash == head)
        .map(|run| run.id)
        .collect();
    head_run_ids.sort_unstable();
    let newest_head_run = head_run_ids.last().copied();
    let remote_head = match &remote {
        Ok(Some(remote)) => Some(remote.as_str()),
        _ => None,
    };
    let blocked_reason = match &remote {
        Err(_) => Some("remote_branch_head_unavailable"),
        Ok(None) => Some("branch_not_published"),
        Ok(Some(remote)) if remote != &head => Some("checkout_head_is_not_remote_branch_head"),
        Ok(Some(_)) if newest_head_run.is_none() => Some("no_run_observed_for_head"),
        Ok(Some(_)) => None,
    };

    let mut candidates = Vec::new();
    let mut any_failed = false;
    for run in runs.iter().filter(|run| {
        run.branch == branch
            && run.event == "push"
            && run.commit_hash != head
            && run.gate_state() == CiGateState::Running
    }) {
        let skip_reason = if !matches!(run.status.as_str(), "queued" | "running")
            || run.conclusion.is_some()
            || run.finished_at.is_some()
        {
            Some("lifecycle_not_in_flight")
        } else if !summary_commit_valid(&run.commit_hash) {
            Some("commit_hash_not_full")
        } else if newest_head_run.is_some_and(|newest| run.id > newest) {
            Some("newer_than_head_run")
        } else {
            None
        };
        let eligible = skip_reason.is_none() && blocked_reason.is_none();
        let mut candidate = SupersededCandidateJson {
            run_id: run.id,
            commit_hash: run.commit_hash.clone(),
            event: run.event.clone(),
            status: run.status.clone(),
            workflow_name: run.workflow_name.clone(),
            run_url: api.run_url(run.id)?,
            eligible,
            skip_reason,
            action: if !eligible {
                "skipped"
            } else if execute {
                "not_cancelled"
            } else {
                "would_cancel"
            },
            execution_stop: None,
            error: None,
        };
        if eligible && execute {
            match api.cancel_exact_classified(run.id, &run.commit_hash).await {
                Ok(receipt) => {
                    candidate.action = "cancelled";
                    candidate.execution_stop = Some(receipt.execution_stop);
                }
                Err(failure) => {
                    // cancel_exact's errors are redacted by construction; a
                    // failure is never retried here. An unknown outcome is
                    // never reported as a known "not cancelled".
                    any_failed = true;
                    if failure.unknown {
                        candidate.action = "outcome_unknown";
                    }
                    candidate.error = Some(failure.error.to_string());
                }
            }
        }
        candidates.push(candidate);
    }
    candidates.sort_by_key(|candidate| candidate.run_id);

    let eligible_count = candidates.iter().filter(|c| c.eligible).count();
    let json_flag = if json { " --json" } else { "" };
    let mut recommended = Vec::new();
    if !execute && eligible_count > 0 {
        recommended.push(format!("oak ci cancel --superseded --yes{json_flag}"));
    }
    match blocked_reason {
        Some("checkout_head_is_not_remote_branch_head" | "branch_not_published") => {
            recommended.push(format!("oak status{json_flag}"));
        }
        Some("no_run_observed_for_head") => {
            recommended.push(format!("oak ci wait --current{json_flag}"));
        }
        _ => {}
    }
    for candidate in candidates
        .iter()
        .filter(|c| c.action == "cancelled" || c.error.is_some())
    {
        recommended.push(format!(
            "oak ci status --run {} --commit {} --json",
            candidate.run_id, candidate.commit_hash
        ));
    }

    let candidates_empty = candidates.is_empty();
    if json {
        output::print_json(&SupersededJson {
            schema_version: SCHEMA_VERSION,
            mode: "superseded",
            dry_run: !execute,
            branch: &branch,
            head: &head,
            remote_branch_head: remote_head,
            head_run_ids,
            runs_scanned: runs.len(),
            scan_limit: SUPERSEDED_SCAN_LIMIT,
            blocked_reason,
            candidates,
            recommended_next_commands: recommended,
        })?;
    } else {
        let short = &head[..head.len().min(12)];
        if candidates.is_empty() {
            output::info(&format!(
                "No superseded in-flight push runs for {branch}@{short} in the {} most recent runs.",
                runs.len()
            ));
        }
        for candidate in &candidates {
            let detail = match (&candidate.error, candidate.skip_reason) {
                (Some(error), _) => format!(" — {error}"),
                (None, Some(reason)) => format!(" ({reason})"),
                (None, None) => String::new(),
            };
            output::print_line(&format!(
                "#{:<5} {} {}@{}  {}  {}{detail}",
                candidate.run_id,
                candidate.workflow_name,
                branch,
                &candidate.commit_hash[..candidate.commit_hash.len().min(12)],
                candidate.status,
                candidate.action,
            ));
        }
        if let Some(reason) = blocked_reason {
            output::warning(&format!(
                "Nothing will be cancelled: {reason}. The checkout head must be the published branch head with an observed run."
            ));
        } else if !execute && eligible_count > 0 {
            output::info("Dry run: nothing was cancelled. Re-run with --yes to cancel the runs marked would_cancel.");
        }
        if candidates.iter().any(|c| c.action == "outcome_unknown") {
            output::warning(
                "Some cancellation outcomes are unknown (the request may have been recorded). Inspect those runs read-only; do not retry blindly.",
            );
        }
        if candidates.iter().any(|c| c.action == "cancelled") {
            output::warning(
                "Backend execution stop is best effort and is not confirmed by this response.",
            );
        }
        for command in &recommended {
            output::info(command);
        }
    }
    Ok(
        if any_failed || (execute && blocked_reason.is_some() && !candidates_empty) {
            CiGateState::Failure.exit_code()
        } else {
            0
        },
    )
}

/// `oak ci runs [--limit N] [--json]` — recent runs for this repo.
pub async fn runs(path: &Path, limit: usize, json: bool) -> Result<()> {
    let api = CiClient::from_repo(path)?;
    let runs = api.list_runs(limit.max(1)).await?;

    if json {
        // Append `run_url` to each run (append-only schema); a server that
        // already supplies one keeps its value.
        let mut with_urls = Vec::with_capacity(runs.len());
        for run in &runs {
            let mut run = run.clone();
            if !run.extra.contains_key("run_url") {
                run.extra.insert(
                    "run_url".to_string(),
                    serde_json::Value::String(api.run_url(run.id)?),
                );
            }
            with_urls.push(run);
        }
        return output::print_json(&RunsJson {
            schema_version: SCHEMA_VERSION,
            runs: &with_urls,
        });
    }

    if runs.is_empty() {
        // Empty recent list ≠ "push will dispatch": workflows may be absent,
        // and push can be already-up-to-date with published:false.
        output::info(
            "No CI runs in this recent list (bounded; not full history). Absence here does not mean a push or merge will dispatch workflows.",
        );
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
    if expected_commit.is_some_and(|hash| !summary_commit_valid(hash)) {
        return Err(OakError::Server(
            "Expected CI commit must be a full 64-character hash".into(),
        ));
    }
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let api = CiClient::from_open_repo(repo.as_ref())?;
    let identity = checkout_identity(repo.as_ref());
    let checkout = identity.as_ref().ok().cloned();
    let subject = if run_id.is_some() {
        "exact_run"
    } else {
        "current_head"
    };
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
        let (branch, head) = identity?;
        let run = api.latest_run_for_commit(&head).await?;
        (branch, head, run)
    };
    let short = &head[..head.len().min(12)];
    let checkout_matches_observed = observed_is_checkout(checkout.as_ref(), &branch, &head);
    let (checkout_branch, checkout_head) = match &checkout {
        Some((branch, head)) => (Some(branch.as_str()), Some(head.as_str())),
        None => (None, None),
    };

    let Some(run) = run else {
        if json {
            output::print_json(&StatusJson {
                schema_version: SCHEMA_VERSION,
                branch: &branch,
                commit: &head,
                state: "no_runs",
                run: None,
                run_url: None,
                subject,
                checkout_branch,
                checkout_head,
                checkout_matches_observed,
                // Missing run ≠ unpublished work: `oak push` can be
                // already-up-to-date with published:false and still not
                // dispatch CI. Inspect at least as many recent runs as
                // status already scanned (STATUS_SCAN_LIMIT).
                recommended_next_commands: vec![format!(
                    "oak ci runs --json --limit {STATUS_SCAN_LIMIT}"
                )],
            })?;
        } else {
            output::warning(&format!(
                "No CI runs found for {branch}@{short} — list recent runs with `oak ci runs --json --limit {STATUS_SCAN_LIMIT}` (bounded recent list, not full history)."
            ));
        }
        return Ok(CiGateState::Failure.exit_code());
    };

    let state = run.gate_state();
    let success_next = || success_advice(checkout.as_ref(), &[(&branch, &head)], json);
    if json {
        let recommended = match state {
            CiGateState::Success => success_next(),
            CiGateState::Failure => vec![
                format!("oak ci logs {}", run.id),
                format!("oak ci rerun {}", run.id),
            ],
            CiGateState::Running => vec![
                wait_command(&[run.id], Some(&head), true, false, None),
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
            subject,
            checkout_branch,
            checkout_head,
            checkout_matches_observed,
            recommended_next_commands: recommended,
        })?;
        return Ok(state.exit_code());
    }

    output::print_line(&run.summary_line());
    match state {
        CiGateState::Success if checkout_matches_observed => {
            output::success(&format!(
                "CI passed for {branch}@{short} — `oak merge` is unblocked."
            ));
        }
        CiGateState::Success => {
            output::success(&format!(
                "CI passed for {branch}@{short} (run #{}).",
                run.id
            ));
            let claim = match &checkout {
                Some((checkout_branch, checkout_head)) if !branch.is_empty() => format!(
                    "This run is not the current checkout head (the current checkout is {checkout_branch}@{})",
                    &checkout_head[..checkout_head.len().min(12)]
                ),
                _ => "Could not confirm this run is the current checkout head (the checkout or run subject could not be established)".to_string(),
            };
            output::warning(&format!(
                "{claim}; it does not validate a bare `oak merge`. Next: {}.",
                success_next()
                    .iter()
                    .map(|command| format!("`{command}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
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
                "CI is still running for {branch}@{short} — wait read-only with `{}`.",
                wait_command(&[run.id], Some(&head), false, false, None)
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
    let recommended_next_commands = rerun_wait_advice(
        &receipt.run_ids,
        &old.commit_hash,
        requested_commit_confirmed,
        json,
    );

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

/// Opt-in progress reporting for `oak ci wait` (fb-326/352/406/472).
///
/// Progress is derived only from the run/job/step metadata the wait already
/// polls: status and conclusion transitions, never log text, and never test
/// counts (the server does not report any). Every event goes to stderr so
/// stdout keeps the single final document `--json` consumers parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProgressMode {
    /// No progress output (the legacy contract).
    #[default]
    Off,
    /// Human-readable lines on stderr.
    Text,
    /// One JSON object per line (JSONL) on stderr.
    Jsonl,
}

/// Knobs shared by exact-run and current-head waits.
#[derive(Debug, Clone, Copy)]
pub struct WaitOptions {
    pub timeout: std::time::Duration,
    pub json: bool,
    pub summary: bool,
    pub progress: ProgressMode,
}

/// Hard cap on progress events per invocation, heartbeats included. A final
/// `progress_truncated` event marks the cap so silence is never ambiguous.
const MAX_PROGRESS_EVENTS: usize = 200;
/// Minimum quiet interval before a heartbeat restates what is still running.
const PROGRESS_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);
/// Poll interval while waiting for a current-head run to be dispatched. The
/// runs list is heavier than one run's metadata, so it is polled less often.
const DISPATCH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
/// Default bound on how long `--current` waits for a run to appear.
pub const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 120;

/// Workflow-defined names are shown in progress output, bounded and stripped
/// of control characters so a hostile name cannot drive the terminal.
fn progress_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
    if cleaned.is_empty() {
        "-".to_string()
    } else {
        cleaned
    }
}

#[derive(Serialize)]
struct ProgressEvent<'a> {
    schema_version: u32,
    event: &'a str,
    /// Whole seconds since this wait started (observation time, not CI time).
    elapsed_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_ids: Option<&'a [u64]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_name: Option<String>,
    /// 1-based position of the step within its job, and the job's step count.
    #[serde(skip_serializing_if = "Option::is_none")]
    step_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_state: Option<&'static str>,
}

impl<'a> ProgressEvent<'a> {
    fn new(event: &'a str, elapsed_secs: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event,
            elapsed_secs,
            run_id: None,
            run_ids: None,
            commit: None,
            job_id: None,
            job_name: None,
            step_id: None,
            step_name: None,
            step_index: None,
            step_count: None,
            status: None,
            conclusion: None,
            exit_code: None,
            gate_state: None,
        }
    }

    fn text(&self) -> String {
        let at = format_duration_secs(self.elapsed_secs as i64);
        let outcome = match (&self.status, &self.conclusion) {
            (_, Some(conclusion)) => conclusion.clone(),
            (Some(status), None) => status.clone(),
            (None, None) => String::new(),
        };
        let exit = self
            .exit_code
            .map(|code| format!(" (exit {code})"))
            .unwrap_or_default();
        match self.event {
            "step" => format!(
                "ci progress +{at}: run #{} {} step {}/{} {}: {outcome}{exit}",
                self.run_id.unwrap_or_default(),
                self.job_name.as_deref().unwrap_or("-"),
                self.step_index.unwrap_or_default(),
                self.step_count.unwrap_or_default(),
                self.step_name.as_deref().unwrap_or("-"),
            ),
            "run" => format!(
                "ci progress +{at}: run #{} {outcome}",
                self.run_id.unwrap_or_default()
            ),
            "heartbeat" => format!(
                "ci progress +{at}: still waiting on run #{}{}",
                self.run_id.unwrap_or_default(),
                match (&self.job_name, &self.step_name) {
                    (Some(job), Some(step)) => format!(
                        " ({job} step {}/{} {step} {outcome})",
                        self.step_index.unwrap_or_default(),
                        self.step_count.unwrap_or_default()
                    ),
                    _ => format!(" ({outcome})"),
                }
            ),
            "dispatch_pending" => format!(
                "ci progress +{at}: no CI run observed yet for commit {}; waiting for dispatch",
                self.commit.map(|c| &c[..c.len().min(12)]).unwrap_or("-")
            ),
            "bound" => format!(
                "ci progress +{at}: bound to run(s) {} for commit {}",
                self.run_ids
                    .unwrap_or_default()
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.commit.map(|c| &c[..c.len().min(12)]).unwrap_or("-")
            ),
            "progress_truncated" => format!(
                "ci progress +{at}: progress event limit ({MAX_PROGRESS_EVENTS}) reached; further progress suppressed until the final result"
            ),
            other => format!("ci progress +{at}: {other}"),
        }
    }
}

#[derive(Default)]
struct RunProgress {
    state: (String, Option<String>),
    steps: std::collections::HashMap<(u64, u64), (String, Option<String>)>,
}

/// Tracks observed run/step state and emits bounded transition events.
struct ProgressTracker {
    mode: ProgressMode,
    started: tokio::time::Instant,
    last_emit: tokio::time::Instant,
    emitted: usize,
    truncated: bool,
    runs: std::collections::HashMap<u64, RunProgress>,
}

impl ProgressTracker {
    fn new(mode: ProgressMode, started: tokio::time::Instant) -> Self {
        Self {
            mode,
            started,
            last_emit: started,
            emitted: 0,
            truncated: false,
            runs: std::collections::HashMap::new(),
        }
    }

    fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn emit(&mut self, event: &ProgressEvent<'_>) {
        if self.mode == ProgressMode::Off || self.truncated {
            return;
        }
        if self.emitted >= MAX_PROGRESS_EVENTS {
            self.truncated = true;
            let marker = ProgressEvent::new("progress_truncated", self.elapsed_secs());
            self.write(&marker);
            return;
        }
        self.emitted += 1;
        self.last_emit = tokio::time::Instant::now();
        self.write(event);
    }

    fn write(&self, event: &ProgressEvent<'_>) {
        use std::io::Write as _;
        let line = match self.mode {
            ProgressMode::Off => return,
            ProgressMode::Text => event.text(),
            ProgressMode::Jsonl => match serde_json::to_string(event) {
                Ok(line) => line,
                Err(_) => return,
            },
        };
        // Progress is advisory: a closed stderr must not abort the wait or
        // change its exit code, so write errors are deliberately ignored.
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }

    fn dispatch_pending(&mut self, commit: &str) {
        let mut event = ProgressEvent::new("dispatch_pending", self.elapsed_secs());
        event.commit = Some(commit);
        self.emit(&event);
    }

    fn bound(&mut self, run_ids: &[u64], commit: &str) {
        let mut event = ProgressEvent::new("bound", self.elapsed_secs());
        event.run_ids = Some(run_ids);
        event.commit = Some(commit);
        self.emit(&event);
    }

    /// Record one polled observation and emit its transitions. The first
    /// observation of a run reports the run state plus any step already
    /// running or concluded non-successfully; later observations report
    /// every step whose status or conclusion changed.
    fn observe(&mut self, run: &CiRun) {
        if self.mode == ProgressMode::Off {
            return;
        }
        let first = !self.runs.contains_key(&run.id);
        let previous = self.runs.remove(&run.id).unwrap_or_default();
        let mut next = RunProgress {
            state: (run.status.clone(), run.conclusion.clone()),
            steps: std::collections::HashMap::new(),
        };
        let elapsed = self.elapsed_secs();
        let mut events = Vec::new();
        for job in run.jobs.iter().flatten() {
            for (index, step) in job.steps.iter().enumerate() {
                let key = (job.id, step.id);
                let state = (step.status.clone(), step.conclusion.clone());
                let changed = match previous.steps.get(&key) {
                    Some(old) => *old != state,
                    None if first => {
                        step.status == "running"
                            || step
                                .conclusion
                                .as_deref()
                                .is_some_and(|c| !matches!(c, "success" | "skipped"))
                    }
                    None => step.status != "queued",
                };
                if changed {
                    let mut event = ProgressEvent::new("step", elapsed);
                    event.run_id = Some(run.id);
                    event.job_id = Some(job.id);
                    event.job_name = Some(progress_name(&job.name));
                    event.step_id = Some(step.id);
                    event.step_name = Some(progress_name(&step.name));
                    event.step_index = Some(index + 1);
                    event.step_count = Some(job.steps.len());
                    event.status = Some(progress_name(&step.status));
                    event.conclusion = step.conclusion.as_deref().map(progress_name);
                    event.exit_code = step.exit_code;
                    events.push(event);
                }
                next.steps.insert(key, state);
            }
        }
        let run_changed = first || previous.state != next.state;
        if run_changed {
            let mut event = ProgressEvent::new("run", elapsed);
            event.run_id = Some(run.id);
            event.status = Some(progress_name(&run.status));
            event.conclusion = run.conclusion.as_deref().map(progress_name);
            event.gate_state = Some(run.gate_state().as_str());
            // Run-level state leads on first sight and on start; terminal
            // conclusions follow the step transitions that caused them.
            if run.gate_state() == CiGateState::Running {
                events.insert(0, event);
            } else {
                events.push(event);
            }
        }
        self.runs.insert(run.id, next);
        for event in &events {
            self.emit(event);
        }
    }

    /// Restate what is still running after a quiet interval, so a long step
    /// is visibly alive without flooding the stream.
    fn heartbeat(&mut self, runs: &std::collections::BTreeMap<u64, CiRun>) {
        if self.mode == ProgressMode::Off
            || self.truncated
            || self.last_emit.elapsed() < PROGRESS_HEARTBEAT
        {
            return;
        }
        let elapsed = self.elapsed_secs();
        let mut events = Vec::new();
        for run in runs
            .values()
            .filter(|run| run.gate_state() == CiGateState::Running)
        {
            let mut event = ProgressEvent::new("heartbeat", elapsed);
            event.run_id = Some(run.id);
            event.status = Some(progress_name(&run.status));
            let current = run.jobs.iter().flatten().find_map(|job| {
                job.steps
                    .iter()
                    .enumerate()
                    .find(|(_, step)| step.status == "running")
                    .map(|(index, step)| (job, index, step))
            });
            if let Some((job, index, step)) = current {
                event.job_id = Some(job.id);
                event.job_name = Some(progress_name(&job.name));
                event.step_id = Some(step.id);
                event.step_name = Some(progress_name(&step.name));
                event.step_index = Some(index + 1);
                event.step_count = Some(job.steps.len());
                event.status = Some(progress_name(&step.status));
            }
            events.push(event);
        }
        for event in &events {
            self.emit(event);
        }
        // Even with nothing running, do not re-check every poll.
        self.last_emit = tokio::time::Instant::now();
    }
}

/// How a `--current` wait resolved its target, reported alongside the
/// normal wait document so the binding is auditable.
#[derive(Serialize, Clone)]
struct CurrentTargetJson {
    /// Always "current_head": the checkout head resolved once at start.
    subject: &'static str,
    current_branch: String,
    /// How the bound runs were chosen, once, never re-resolved:
    /// "latest_run_per_workflow_for_exact_commit_on_current_branch" (runs
    /// recorded on this branch) or "other_branch_same_commit" (no run on this
    /// branch appeared within the dispatch bound; the bound runs are the
    /// newest per workflow for the same commit on other branches, e.g. a
    /// fresh branch cut from a main commit). Neither is publication-aware
    /// like the server merge gate, and a workflow dispatched after binding
    /// is not included; success is an observation, not landing authority.
    binding: &'static str,
    /// Branches the bound runs were recorded on, when any differs from
    /// `current_branch`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bound_branches: Vec<String>,
    dispatch_waited_ms: u64,
    runs_scanned: usize,
    scan_limit: usize,
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
    wait_with_summary(
        path,
        requested_run_ids,
        expected_commit,
        timeout,
        json,
        false,
    )
    .await
}

/// Opt-in summary projection. The default wait contract remains unchanged.
pub async fn wait_with_summary(
    path: &Path,
    requested_run_ids: &[u64],
    expected_commit: Option<&str>,
    timeout: std::time::Duration,
    json: bool,
    summary: bool,
) -> Result<i32> {
    wait_with_options(
        path,
        requested_run_ids,
        expected_commit,
        WaitOptions {
            timeout,
            json,
            summary,
            progress: ProgressMode::Off,
        },
    )
    .await
}

fn validate_wait_options(options: &WaitOptions, expected_commit: Option<&str>) -> Result<()> {
    if options.summary
        && (!options.json || expected_commit.is_some_and(|c| !summary_commit_valid(c)))
    {
        return Err(OakError::InvalidArgument(
            "CI wait summary requires --json and, if supplied, a full 64-digit commit hash".into(),
        ));
    }
    Ok(())
}

/// Exact-run wait with every option, including opt-in progress.
pub async fn wait_with_options(
    path: &Path,
    requested_run_ids: &[u64],
    expected_commit: Option<&str>,
    options: WaitOptions,
) -> Result<i32> {
    validate_wait_options(&options, expected_commit)?;
    if requested_run_ids.is_empty() {
        return Err(OakError::InvalidArgument(
            "oak ci wait requires at least one run id (or --current)".to_string(),
        ));
    }
    if requested_run_ids.contains(&0) {
        return Err(OakError::InvalidArgument(
            "oak ci wait requires non-zero run ids".to_string(),
        ));
    }
    let api = CiClient::from_repo(path)?;
    let started = tokio::time::Instant::now();
    let mut progress = ProgressTracker::new(options.progress, started);
    wait_exact(
        path,
        &api,
        requested_run_ids,
        expected_commit,
        options,
        started,
        &mut progress,
        None,
    )
    .await
}

pub(crate) const BINDING_CURRENT_BRANCH: &str =
    "latest_run_per_workflow_for_exact_commit_on_current_branch";
pub(crate) const BINDING_OTHER_BRANCH: &str = "other_branch_same_commit";

/// Newest run per workflow for exactly `commit`, optionally restricted to
/// runs recorded on `branch`. Like the server gate's per-workflow query but
/// NOT publication-aware. Returned ids are sorted.
pub(crate) fn current_head_run_ids(runs: &[CiRun], commit: &str, branch: Option<&str>) -> Vec<u64> {
    let mut newest = std::collections::BTreeMap::<&str, u64>::new();
    for run in runs
        .iter()
        .filter(|run| run.commit_hash == commit && branch.is_none_or(|b| run.branch == b))
    {
        let workflow = run
            .workflow_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(run.workflow_name.as_str());
        let entry = newest.entry(workflow).or_insert(run.id);
        *entry = (*entry).max(run.id);
    }
    let mut ids: Vec<u64> = newest.into_values().filter(|id| *id > 0).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[derive(Serialize)]
struct DispatchPendingJson<'a> {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection: Option<&'static str>,
    requested_run_ids: [u64; 0],
    expected_commit: &'a str,
    /// "dispatch_pending": no run for the exact head was observed in time.
    state: &'static str,
    observations: [u64; 0],
    #[serde(flatten)]
    current: CurrentTargetJson,
    /// The remote branch head, when it could be read. `head_published`
    /// false means no run for this head should be expected until a push.
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_branch_head: Option<String>,
    /// true / false when the remote head was read; absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_published: Option<bool>,
    note: &'static str,
    recommended_next_commands: Vec<String>,
}

/// Best-effort remote branch head: `Ok(None)` means the branch is not on the
/// remote; an error means the head is unknown.
async fn remote_branch_head(
    api: &CiClient,
    branch: &str,
    budget: std::time::Duration,
) -> Result<Option<String>> {
    let client = crate::http::api_client();
    let endpoint = format!("{}/{}", api.owner, api.repo);
    let head = tokio::time::timeout(
        budget,
        super::push::fetch_remote_branch_head(
            &client,
            api.remote.trim_end_matches('/'),
            &endpoint,
            branch,
            api.token.as_deref(),
        ),
    )
    .await
    .map_err(|_| OakError::Http("remote branch head request timed out".into()))??;
    Ok(head.map(|hash| hash.to_string()))
}

/// `oak ci wait --current`: resolve the checkout's branch head once, bind
/// to the newest run per workflow for exactly that commit, preferring runs
/// on this branch (waiting a bounded time for dispatch; other-branch
/// same-commit runs are bound only after that, and labelled), then wait on
/// those exact runs with the head as the `--commit` fence. It never
/// re-resolves the head or follows a newer run. Not publication-aware and
/// blind to workflows dispatched after binding (see `CurrentTargetJson`).
pub async fn wait_current(
    path: &Path,
    options: WaitOptions,
    dispatch_timeout: std::time::Duration,
) -> Result<i32> {
    validate_wait_options(&options, None)?;
    let (api, branch, head) = {
        let ctx = crate::resolve::resolve(path)?;
        let repo = ctx.open()?;
        let api = CiClient::from_open_repo(repo.as_ref())?;
        let (branch, head) = checkout_identity(repo.as_ref())?;
        (api, branch, head)
    };
    let started = tokio::time::Instant::now();
    let mut progress = ProgressTracker::new(options.progress, started);
    let one_shot = options.timeout.is_zero() || dispatch_timeout.is_zero();
    // Overall I/O budget: the same one-second ceiling as a one-shot probe.
    let overall_deadline = started
        + if options.timeout.is_zero() {
            std::time::Duration::from_secs(1)
        } else {
            options.timeout
        };
    let dispatch_deadline = (started + dispatch_timeout.min(options.timeout)).min(overall_deadline);
    let mut announced_pending = false;
    let mut runs_scanned;
    let mut binding = BINDING_CURRENT_BRANCH;
    let mut bound_branches = Vec::new();
    let bound = loop {
        let runs = match tokio::time::timeout_at(overall_deadline, api.list_runs(STATUS_SCAN_LIMIT))
            .await
        {
            Ok(result) => Some(result?),
            Err(_) => None,
        };
        let list_timed_out = runs.is_none();
        runs_scanned = runs.as_ref().map_or(0, Vec::len);
        let runs = runs.unwrap_or_default();
        // Prefer runs recorded on this branch: a same-commit run on another
        // branch may be an older publication's (or main's) result.
        let ids = current_head_run_ids(&runs, &head, Some(&branch));
        if !ids.is_empty() {
            break ids;
        }
        let now = tokio::time::Instant::now();
        if list_timed_out || one_shot || now >= dispatch_deadline {
            // Only after the dispatch bound: bind same-commit runs from other
            // branches, labelled explicitly, rather than report nothing.
            let other = current_head_run_ids(&runs, &head, None);
            if !other.is_empty() {
                binding = BINDING_OTHER_BRANCH;
                for run in runs.iter().filter(|run| other.contains(&run.id)) {
                    if !bound_branches.contains(&run.branch) {
                        bound_branches.push(run.branch.clone());
                    }
                }
                bound_branches.sort();
            }
            break other;
        }
        if !announced_pending {
            progress.dispatch_pending(&head);
            announced_pending = true;
        }
        tokio::time::sleep_until(dispatch_deadline.min(now + DISPATCH_POLL_INTERVAL)).await;
    };
    let current = CurrentTargetJson {
        subject: "current_head",
        current_branch: branch.clone(),
        binding,
        bound_branches,
        dispatch_waited_ms: started.elapsed().as_millis() as u64,
        runs_scanned,
        scan_limit: STATUS_SCAN_LIMIT,
    };

    if bound.is_empty() {
        // Diagnostic only: a short, separate budget so an unknown remote
        // head degrades to `head_published` absent rather than a long stall.
        // Bounded by what is left of --timeout too, so `--timeout 0` stays a
        // one-second probe; an exhausted budget reports the head as unknown.
        let budget = std::time::Duration::from_secs(10)
            .min(overall_deadline.saturating_duration_since(tokio::time::Instant::now()));
        let remote = if budget.is_zero() {
            Err(OakError::Http(
                "no time left to read the remote branch head".into(),
            ))
        } else {
            remote_branch_head(&api, &branch, budget).await
        };
        let (remote_branch_head, head_published) = match &remote {
            Ok(Some(remote)) => (Some(remote.clone()), Some(remote == &head)),
            Ok(None) => (None, Some(false)),
            Err(_) => (None, None),
        };
        let json_flag = if options.json { " --json" } else { "" };
        let summary_flag = if options.summary { " --summary" } else { "" };
        let mut recommended = Vec::new();
        if head_published == Some(false) {
            recommended.push(format!("oak push{json_flag}"));
        }
        recommended.push(format!("oak ci wait --current{json_flag}{summary_flag}"));
        recommended.push(format!("oak ci runs --json --limit {STATUS_SCAN_LIMIT}"));
        let note = "No run for the exact checkout head was observed within the dispatch bound. This is not a CI result: dispatch may still be pending, the head may be unpublished, or no workflow matched. The wait never binds to a run for another commit.";
        if options.json {
            output::print_json(&DispatchPendingJson {
                schema_version: SCHEMA_VERSION,
                projection: options.summary.then_some("summary"),
                requested_run_ids: [],
                expected_commit: &head,
                state: "dispatch_pending",
                observations: [],
                current,
                remote_branch_head,
                head_published,
                note,
                recommended_next_commands: recommended,
            })?;
        } else {
            output::warning(&format!(
                "No CI run observed for {branch}@{} after {}s (scanned the {runs_scanned} most recent runs). {}",
                &head[..head.len().min(12)],
                started.elapsed().as_secs(),
                match head_published {
                    Some(false) => "The checkout head is not the published branch head; push it first.",
                    Some(true) => "Dispatch may still be pending, or no workflow matched this head.",
                    None => "Dispatch may still be pending, the head may be unpublished, or no workflow matched.",
                }
            ));
            for command in &recommended {
                output::info(command);
            }
        }
        // Not a CI conclusion: report "not yet known", the same retryable
        // code as a wait timeout.
        return Ok(CiGateState::Running.exit_code());
    }

    progress.bound(&bound, &head);
    let remaining = if options.timeout.is_zero() {
        std::time::Duration::ZERO
    } else {
        options.timeout.saturating_sub(started.elapsed())
    };
    wait_exact(
        path,
        &api,
        &bound,
        Some(&head),
        WaitOptions {
            timeout: remaining,
            ..options
        },
        tokio::time::Instant::now(),
        &mut progress,
        Some(current),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_exact(
    path: &Path,
    api: &CiClient,
    requested_run_ids: &[u64],
    expected_commit: Option<&str>,
    options: WaitOptions,
    started: tokio::time::Instant,
    progress: &mut ProgressTracker,
    current: Option<CurrentTargetJson>,
) -> Result<i32> {
    let WaitOptions {
        timeout,
        json,
        summary,
        ..
    } = options;
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
    let mut observed_at = std::collections::BTreeMap::<u64, String>::new();

    loop {
        let mut request_timed_out = false;
        for requested in requested_run_ids {
            if observed
                .get(requested)
                .is_some_and(|run| run.gate_state() != CiGateState::Running)
            {
                continue;
            }
            let run = match tokio::time::timeout_at(
                deadline,
                api.get_run_with_redaction(*requested, summary),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    request_timed_out = true;
                    break;
                }
            };
            if summary && !summary_commit_valid(&run.commit_hash) {
                return Err(OakError::Server(
                    "CI wait summary requires a full source commit identity; remote value omitted"
                        .into(),
                ));
            }
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
            progress.observe(&run);
            observed_at.insert(*requested, chrono::Utc::now().to_rfc3339());
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
            let mut observations = Vec::new();
            for requested in requested_run_ids {
                if let Some(run) = observed.get(requested) {
                    observations.push(WaitObservation {
                        requested_run_id: *requested,
                        observed_run_id: run.id,
                        observed_commit: &run.commit_hash,
                        state: run.gate_state().as_str(),
                        run_url: api.run_url(run.id)?,
                        run,
                    });
                }
            }
            let recommended_next_commands = match state {
                // Summary never advises a merge; plain text prints no advice.
                "success" if summary || !json => Vec::new(),
                // Read the checkout identity now, not before the poll loop:
                // a commit or switch during the wait must not leave a stale
                // match that recommends merging something else.
                "success" => success_advice(
                    current_checkout_identity(path).as_ref(),
                    &observed
                        .values()
                        .map(|run| (run.branch.as_str(), run.commit_hash.as_str()))
                        .collect::<Vec<_>>(),
                    json,
                ),
                "failure" => observed
                    .values()
                    .filter(|run| run.gate_state() == CiGateState::Failure)
                    .map(|run| {
                        if summary {
                            format!("oak ci logs {} --failed --max-bytes 65536 --json", run.id)
                        } else {
                            format!("oak ci logs {}", run.id)
                        }
                    })
                    .collect(),
                _ => vec![wait_command(
                    requested_run_ids,
                    expected_commit,
                    json,
                    summary,
                    None,
                )],
            };

            if summary {
                let mut summary_observations = Vec::new();
                for id in requested_run_ids {
                    if let (Some(run), Some(at)) = (observed.get(id), observed_at.get(id)) {
                        summary_observations.push(wait_summary_observation(
                            run,
                            at,
                            api.run_url(run.id)?,
                        ));
                    }
                }
                output::print_json(&WaitSummaryJson {
                    schema_version: SCHEMA_VERSION,
                    projection: "summary",
                    provider: "oak_ci",
                    execution_backend: "unavailable_in_summary",
                    transfer_scope: "legacy_full_run_details",
                    requested_run_ids,
                    expected_commit,
                    state,
                    observations: summary_observations,
                    current,
                    recommended_next_commands,
                })?;
            } else if json {
                output::print_json(&WaitJson {
                    schema_version: SCHEMA_VERSION,
                    requested_run_ids,
                    expected_commit,
                    state,
                    observations,
                    current,
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

        progress.heartbeat(&observed);
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
    fn current_head_binding_is_newest_run_per_workflow_for_exact_commit() {
        let mk = |id: u64, workflow: &str, commit: &str| CiRun {
            id,
            workflow_name: workflow.to_string(),
            workflow_path: Some(format!(".oak/workflows/{workflow}.yml")),
            commit_hash: commit.to_string(),
            ..CiRun::default()
        };
        let runs = vec![
            mk(9, "ci", "other"),
            mk(8, "lint", "head"),
            mk(7, "ci", "head"),
            mk(5, "ci", "head"),
        ];
        assert_eq!(current_head_run_ids(&runs, "head", None), vec![7, 8]);
        assert!(current_head_run_ids(&runs, "missing", None).is_empty());
        let mut on_branch = runs.clone();
        on_branch[1].branch = "feature".into();
        on_branch[3].branch = "feature".into();
        assert_eq!(
            current_head_run_ids(&on_branch, "head", Some("feature")),
            vec![5, 8]
        );
    }

    #[tokio::test]
    async fn progress_events_are_capped_with_one_truncation_marker() {
        let mut tracker = ProgressTracker::new(ProgressMode::Jsonl, tokio::time::Instant::now());
        for _ in 0..(MAX_PROGRESS_EVENTS + 50) {
            tracker.dispatch_pending(&"a".repeat(64));
        }
        assert_eq!(tracker.emitted, MAX_PROGRESS_EVENTS);
        assert!(tracker.truncated);
        let mut off = ProgressTracker::new(ProgressMode::Off, tokio::time::Instant::now());
        off.dispatch_pending("x");
        assert_eq!(off.emitted, 0);
    }

    #[test]
    fn progress_names_are_bounded_and_control_free() {
        assert_eq!(progress_name("te\u{1b}[31mst"), "te[31mst");
        assert_eq!(progress_name(&"x".repeat(500)).len(), 80);
        assert_eq!(progress_name(""), "-");
    }

    #[test]
    fn duration_formats_compactly() {
        assert_eq!(format_duration_secs(45), "45s");
        assert_eq!(format_duration_secs(382), "6m22s");
        assert_eq!(format_duration_secs(3725), "1h02m");
        assert_eq!(format_duration_secs(-1), "-");
    }
}
