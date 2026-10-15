//! `oak ci deploy` — authorize one run of a protected environment's release
//! workflow.
//!
//! A protected environment (configured on the server: the branches that may
//! deploy, pinned workflows, reviewers, bound runners) only ever runs a
//! workflow through an explicit *release authorization*, never on a push.
//! This command is the CLI half of that flow:
//!
//! 1. `GET /api/:owner/:repo/ci/environments/:env/release-preview
//!    ?workflow=&branch=` — the exact values an authorization must carry (the
//!    branch head commit, the workflow path and blob hash, the event, the
//!    environment's policy version), who may approve it, whether a bound
//!    runner is online, and every problem that would stop it.
//! 2. After confirmation (`--yes`, or an interactive `[y/N]`),
//!    `POST /api/:owner/:repo/ci/release-authorizations` with those values
//!    copied verbatim. The server re-checks each one and refuses (HTTP 409)
//!    if the branch head, the workflow source, or the policy moved after the
//!    preview, so what was shown is exactly what runs.
//!
//! `--wait` then waits on the created run through the same exact-run wait as
//! `oak ci wait RUN_ID --commit HASH` (fenced to the authorized commit).
//! `--json` emits one document carrying the preview, the authorization
//! receipt, and with `--wait` the embedded `oak ci wait --json --summary`
//! document.
//!
//! Remote resolution, repository identity, and credentials are
//! [`CiClient`]'s, exactly as for every other `oak ci` subcommand. An API key
//! must carry the server's explicit `release` scope; the server enforces that
//! and its refusal is surfaced as-is.
//!
//! ## Exit codes
//!
//! - `0` — authorized (with `--wait`: and the run passed), or a `--dry-run`
//!   that a real deploy would have proceeded past.
//! - `1` — the preview is not `ready` (its `problems` say why), the prompt
//!   was declined, or (with `--wait`) the run failed. "Not ready" is the
//!   generic code on purpose, not `6`: the server answered and understood
//!   the request; what blocks the release is environment state (an offline
//!   runner, a disabled environment, a stale workflow pin) that someone has
//!   to fix. `6` would point scripts at retrying or re-authenticating, which
//!   cannot help.
//! - `2` — a confirmation was needed but cannot be asked: no `--yes` under
//!   `--json` or without an interactive terminal. That is a usage error
//!   (pass `--yes`), like any other missing required flag.
//! - `3` — `--wait` timed out with the run still in flight.
//! - `6` — network, server, or authorization failure. That includes a server
//!   refusal of the authorization (409/403/503) and a preview saying the
//!   caller cannot approve this environment (`can_authorize: false`): the
//!   latter is the same refusal the POST would return as an HTTP error,
//!   which the CLI maps to `6` everywhere, so detecting it early must not
//!   change the code.

use std::path::Path;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use super::{
    bounded_ci_body, parse_json_lenient, wait_command, wait_exact, CiClient, ProgressMode,
    ProgressTracker, WaitDelivery, WaitOptions, CI_MUTATION_TIMEOUT, SCHEMA_VERSION,
};
use crate::output;

/// Longest server-supplied name this command echoes or puts in a command.
const MAX_NAME_LEN: usize = 255;

// ---------------------------------------------------------------------------
// Wire types (mirror oakspace's `api::ci_environments::ReleasePreviewResponse`
// and `api::ci::ReleaseAuthorization{Request,Response}`)
// ---------------------------------------------------------------------------

/// The server's release preview. Parsed leniently like every CI response:
/// missing fields take their defaults — so a missing `ready` or
/// `can_authorize` reads as `false`, never as permission — and unknown
/// fields pass through to `--json` untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReleasePreview {
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub workflow_path: String,
    #[serde(default)]
    pub workflow_hash: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub policy_version: i64,
    #[serde(default)]
    pub required_reviewers: bool,
    #[serde(default)]
    pub reviewers: Vec<String>,
    #[serde(default)]
    pub runner_online: bool,
    /// Whether the caller may authorize this release.
    #[serde(default)]
    pub can_authorize: bool,
    /// No `problems`: an authorized release would start.
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub problems: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `POST …/ci/release-authorizations` body: the preview's values, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseAuthorizationRequest {
    pub environment: String,
    pub branch: String,
    pub commit: String,
    pub workflow_path: String,
    pub workflow_hash: String,
    pub event: String,
    pub policy_version: i64,
}

/// `POST …/ci/release-authorizations` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAuthorization {
    pub authorization_id: String,
    pub run_id: u64,
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A name safe to echo on a terminal and to put in a recommended command.
fn plain_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_LEN && !value.chars().any(char::is_control)
}

/// Server-supplied free text (problems, reviewers) for a terminal: control
/// characters dropped so a hostile value cannot drive the terminal.
fn display(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(500)
        .collect()
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

impl ReleasePreview {
    /// The authorization body this preview stands for, after checking that
    /// it is well-formed and answers the question that was asked. The server
    /// re-validates everything; this only keeps a malformed or unrelated
    /// preview from being shown as deployable or sent back.
    pub fn authorization_request(
        &self,
        environment: &str,
        workflow: Option<&str>,
        branch: Option<&str>,
    ) -> Result<ReleaseAuthorizationRequest> {
        let malformed = |what: &str| {
            OakError::Server(format!(
                "The server's release preview is unusable ({what}); nothing was authorized"
            ))
        };
        if !plain_name(&self.environment)
            || !plain_name(&self.branch)
            || !plain_name(&self.workflow_path)
            || !plain_name(&self.event)
            || (!self.workflow.is_empty() && !plain_name(&self.workflow))
        {
            return Err(malformed("missing or invalid names"));
        }
        if !canonical_hash(&self.commit) || !canonical_hash(&self.workflow_hash) {
            return Err(malformed(
                "commit and workflow hash must be full 64-character lowercase hex",
            ));
        }
        if self.policy_version <= 0 {
            return Err(malformed("policy_version must be positive"));
        }
        if !self.environment.eq_ignore_ascii_case(environment) {
            return Err(malformed(&format!(
                "it is for environment '{}', not '{environment}'",
                display(&self.environment)
            )));
        }
        if branch.is_some_and(|branch| branch != self.branch) {
            return Err(malformed(&format!(
                "it is for branch '{}'",
                display(&self.branch)
            )));
        }
        if workflow.is_some_and(|wanted| wanted != self.workflow && wanted != self.workflow_path) {
            return Err(malformed(&format!(
                "it is for workflow '{}'",
                display(&self.workflow_path)
            )));
        }
        Ok(ReleaseAuthorizationRequest {
            environment: self.environment.clone(),
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            workflow_path: self.workflow_path.clone(),
            workflow_hash: self.workflow_hash.clone(),
            event: self.event.clone(),
            policy_version: self.policy_version,
        })
    }

    /// The workflow's display name: its name, else its path.
    fn workflow_label(&self) -> &str {
        if self.workflow.is_empty() {
            &self.workflow_path
        } else {
            &self.workflow
        }
    }
}

// ---------------------------------------------------------------------------
// API calls
// ---------------------------------------------------------------------------

impl CiClient {
    fn ci_url(&self) -> String {
        format!(
            "{}/api/{}/{}/ci",
            self.remote.trim_end_matches('/'),
            self.owner,
            self.repo,
        )
    }

    /// `GET /ci/environments/:env/release-preview` — read-only.
    pub async fn release_preview(
        &self,
        environment: &str,
        workflow: Option<&str>,
        branch: Option<&str>,
    ) -> Result<ReleasePreview> {
        let url = format!(
            "{}/environments/{}/release-preview",
            self.ci_url(),
            urlencoding::encode(environment)
        );
        let mut query = Vec::new();
        if let Some(workflow) = workflow {
            query.push(("workflow", workflow));
        }
        if let Some(branch) = branch {
            query.push(("branch", branch));
        }
        let response = self
            .authed(crate::http::api_client().get(url))
            .query(&query)
            .timeout(CI_MUTATION_TIMEOUT)
            .send()
            .await
            .map_err(|e| OakError::Http(format!("Release preview request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let hint = match status.as_u16() {
                404 => " (check the environment, --workflow, and --branch; a server without protected environments also answers 404)",
                409 => " (pass --workflow NAME to pick one)",
                401 | 403 => " (check the credential with `oak auth status`)",
                _ => "",
            };
            return Err(OakError::Server(format!(
                "Release preview for environment '{environment}' failed: {}{hint}",
                crate::http::error_text(response).await
            )));
        }
        let body = bounded_ci_body(response).await?;
        parse_json_lenient(&body)
    }

    /// `POST /ci/release-authorizations` — authorizes and queues one run.
    /// Not idempotent and never retried: an outcome this request cannot
    /// confirm (network loss, 408, 5xx other than the server's deliberate
    /// 503, a redirect, an unreadable receipt) is reported as unconfirmed.
    pub async fn authorize_release(
        &self,
        request: &ReleaseAuthorizationRequest,
    ) -> Result<ReleaseAuthorization> {
        let unconfirmed = |reason: &str| {
            OakError::Server(format!(
                "Release authorization outcome is unconfirmed ({reason}); a run may have been authorized. Inspect `oak ci runs` before deploying again"
            ))
        };
        let response = self
            .authed(
                crate::http::api_client().post(format!("{}/release-authorizations", self.ci_url())),
            )
            .timeout(CI_MUTATION_TIMEOUT)
            .json(request)
            .send()
            .await
            .map_err(|_| unconfirmed("network failure after sending"))?;
        let status = response.status();
        if !status.is_success() {
            let maybe_recorded = status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status.is_redirection()
                || (status.is_server_error() && status != reqwest::StatusCode::SERVICE_UNAVAILABLE);
            let message = crate::http::error_text(response).await;
            if maybe_recorded {
                return Err(unconfirmed(&message));
            }
            let hint = if status == reqwest::StatusCode::CONFLICT {
                ". Something changed since the preview (branch head, workflow source, or environment policy) or you cannot approve it; run `oak ci deploy` again to review the current values"
            } else {
                ""
            };
            return Err(OakError::Server(format!(
                "Release authorization refused: {message}. Nothing was authorized{hint}"
            )));
        }
        let body = bounded_ci_body(response)
            .await
            .map_err(|_| unconfirmed("the receipt could not be read"))?;
        let receipt: ReleaseAuthorization =
            serde_json::from_str(&body).map_err(|_| unconfirmed("malformed receipt"))?;
        if receipt.run_id == 0
            || receipt.run_id > i64::MAX as u64
            || receipt.authorization_id.is_empty()
            || receipt.authorization_id.len() > 128
            || !receipt
                .authorization_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(unconfirmed(
                "invalid run id or authorization id in the receipt",
            ));
        }
        Ok(receipt)
    }
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// `oak ci deploy` flags.
#[derive(Debug, Clone, Default)]
pub struct DeployOptions {
    pub environment: String,
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub yes: bool,
    pub dry_run: bool,
    /// `--wait[=MIN]`: minutes to wait for the authorized run (`0` polls once).
    pub wait_minutes: Option<u64>,
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Authorized,
    DryRun,
    NotReady,
    CannotAuthorize,
    ConfirmationRequired,
    Declined,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Authorized => "authorized",
            Outcome::DryRun => "dry_run",
            Outcome::NotReady => "not_ready",
            Outcome::CannotAuthorize => "cannot_authorize",
            Outcome::ConfirmationRequired => "confirmation_required",
            Outcome::Declined => "declined",
        }
    }

    /// See the module docs for why each code was chosen.
    fn exit_code(self) -> i32 {
        match self {
            Outcome::Authorized | Outcome::DryRun => 0,
            Outcome::NotReady | Outcome::Declined => 1,
            Outcome::ConfirmationRequired => 2,
            Outcome::CannotAuthorize => 6,
        }
    }
}

/// What stops this preview from being authorized at all, checked in order:
/// environment readiness first (it blocks everyone), then the caller.
fn blocker(preview: &ReleasePreview) -> Option<Outcome> {
    if !preview.ready {
        Some(Outcome::NotReady)
    } else if !preview.can_authorize {
        Some(Outcome::CannotAuthorize)
    } else {
        None
    }
}

#[derive(Serialize)]
struct AuthorizationJson<'a> {
    authorization_id: &'a str,
    run_id: u64,
    run_url: String,
    /// The exact values authorized (the preview's, verbatim).
    request: &'a ReleaseAuthorizationRequest,
}

#[derive(Serialize)]
struct DeployJson<'a> {
    schema_version: u32,
    /// "authorized" | "dry_run" | "not_ready" | "cannot_authorize" |
    /// "confirmation_required"
    outcome: &'static str,
    /// The server's release preview, verbatim.
    preview: &'a ReleasePreview,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization: Option<AuthorizationJson<'a>>,
    /// With `--wait`: the `oak ci wait RUN_ID --commit HASH --json --summary`
    /// document for the authorized run.
    #[serde(skip_serializing_if = "Option::is_none")]
    wait: Option<serde_json::Value>,
    /// With `--wait`: why the wait itself failed (the run was still
    /// authorized; `authorization` says which).
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_error: Option<String>,
    recommended_next_commands: Vec<String>,
}

/// `oak ci deploy` pinned to exactly what `preview` shows (environment,
/// workflow path, branch), plus `flags`. A later run still re-previews: the
/// branch head may move in between.
fn deploy_command(preview: &ReleasePreview, flags: &str) -> String {
    format!(
        "oak ci deploy -e {} --workflow {} --branch {}{flags}",
        output::shell_quote(&preview.environment),
        output::shell_quote(&preview.workflow_path),
        output::shell_quote(&preview.branch),
    )
}

fn validate_options(options: &DeployOptions) -> Result<()> {
    let bad = |what: &str| OakError::InvalidArgument(format!("oak ci deploy: invalid {what}"));
    if !plain_name(&options.environment) || options.environment.contains('/') {
        return Err(bad("--environment"));
    }
    if options.workflow.as_deref().is_some_and(|w| !plain_name(w)) {
        return Err(bad("--workflow"));
    }
    if options.branch.as_deref().is_some_and(|b| !plain_name(b)) {
        return Err(bad("--branch"));
    }
    if options.dry_run && options.wait_minutes.is_some() {
        return Err(OakError::InvalidArgument(
            "oak ci deploy: --wait has nothing to wait for with --dry-run".into(),
        ));
    }
    Ok(())
}

fn print_preview(preview: &ReleasePreview) {
    output::header(&format!(
        "Release preview for environment {}",
        display(&preview.environment)
    ));
    output::detail(
        "branch   ",
        &format!("{}@{}", display(&preview.branch), short(&preview.commit)),
    );
    output::detail(
        "workflow ",
        &format!(
            "{} ({} @ {})",
            display(preview.workflow_label()),
            display(&preview.workflow_path),
            short(&preview.workflow_hash)
        ),
    );
    output::detail("policy   ", &format!("version {}", preview.policy_version));
    let reviewers = if preview.reviewers.is_empty() {
        "none".to_string()
    } else {
        preview
            .reviewers
            .iter()
            .map(|r| display(r))
            .collect::<Vec<_>>()
            .join(", ")
    };
    output::detail(
        "reviewers",
        &format!(
            "{reviewers} ({}; you {})",
            if preview.required_reviewers {
                "approval required"
            } else {
                "writers may approve"
            },
            if preview.can_authorize {
                "can approve"
            } else {
                "cannot approve"
            }
        ),
    );
    output::detail(
        "runner   ",
        if preview.runner_online {
            "online"
        } else {
            "offline"
        },
    );
    if preview.problems.is_empty() {
        output::detail("problems ", "none");
    } else {
        output::detail("problems ", &preview.problems.len().to_string());
        for problem in &preview.problems {
            output::item(&format!("  - {}", display(problem)));
        }
    }
}

fn cannot_authorize_message(preview: &ReleasePreview) -> String {
    let reviewers = preview
        .reviewers
        .iter()
        .map(|r| display(r))
        .collect::<Vec<_>>()
        .join(", ");
    let who = match (preview.required_reviewers, reviewers.is_empty()) {
        (true, true) => "it requires a reviewer and has none configured".to_string(),
        (true, false) => format!("only its reviewers can: {reviewers}"),
        (false, true) => "it needs repository write access".to_string(),
        (false, false) => {
            format!("it needs repository write access or one of its reviewers: {reviewers}")
        }
    };
    format!(
        "You cannot approve releases to '{}': {who}. Nothing was authorized; check which account you are using with `oak auth status`",
        display(&preview.environment)
    )
}

/// Ask `Deploy <workflow> at <branch>@<short> to <env>? [y/N]`. Only called
/// after [`output::can_prompt`] confirmed both streams are a terminal.
fn confirm(preview: &ReleasePreview) -> Result<bool> {
    output::print_raw(&format!(
        "Deploy {} at {}@{} to {}? [y/N] ",
        display(preview.workflow_label()),
        display(&preview.branch),
        short(&preview.commit),
        display(&preview.environment),
    ));
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// `oak ci deploy` — preview, confirm, authorize, and optionally wait.
/// Returns the process exit code (see the module docs).
pub async fn deploy(path: &Path, options: &DeployOptions) -> Result<i32> {
    validate_options(options)?;
    let api = CiClient::from_repo(path)?;
    let preview = api
        .release_preview(
            &options.environment,
            options.workflow.as_deref(),
            options.branch.as_deref(),
        )
        .await?;
    let request = preview.authorization_request(
        &options.environment,
        options.workflow.as_deref(),
        options.branch.as_deref(),
    )?;
    let json_flag = if options.json { " --json" } else { "" };

    if !options.json {
        print_preview(&preview);
    }
    let outcome = match blocker(&preview) {
        Some(blocked) => blocked,
        None if options.dry_run => Outcome::DryRun,
        None if options.yes => Outcome::Authorized,
        // `--json` keeps stdout for its one document, so it never prompts;
        // neither does a session that cannot show the question and read the
        // answer (the same gate `oak reset` uses before discarding work).
        None if options.json || !output::can_prompt() => Outcome::ConfirmationRequired,
        None if confirm(&preview)? => Outcome::Authorized,
        None => Outcome::Declined,
    };

    if outcome != Outcome::Authorized {
        let recommended = match outcome {
            Outcome::DryRun | Outcome::ConfirmationRequired => {
                vec![deploy_command(&preview, &format!(" --yes{json_flag}"))]
            }
            Outcome::NotReady => vec![deploy_command(&preview, &format!(" --dry-run{json_flag}"))],
            Outcome::CannotAuthorize => vec![format!("oak auth status{json_flag}")],
            Outcome::Declined | Outcome::Authorized => Vec::new(),
        };
        if options.json {
            output::print_json(&DeployJson {
                schema_version: SCHEMA_VERSION,
                outcome: outcome.as_str(),
                preview: &preview,
                authorization: None,
                wait: None,
                wait_error: None,
                recommended_next_commands: recommended,
            })?;
            return Ok(outcome.exit_code());
        }
        if !preview.ready {
            output::error(&format!(
                "Release to '{}' is not ready (see problems above); nothing was authorized",
                display(&preview.environment)
            ));
        }
        if !preview.can_authorize {
            output::error(&cannot_authorize_message(&preview));
        }
        match outcome {
            Outcome::DryRun => output::info("Dry run: nothing was authorized."),
            Outcome::ConfirmationRequired => output::error(
                "Refusing to deploy without confirmation: not an interactive terminal. Pass --yes to authorize exactly what the preview shows (or --dry-run to only preview)",
            ),
            Outcome::Declined => output::info("Deploy cancelled; nothing was authorized."),
            _ => {}
        }
        for command in &recommended {
            output::info(command);
        }
        return Ok(outcome.exit_code());
    }

    let receipt = api.authorize_release(&request).await?;
    let run_url = api.run_url(receipt.run_id)?;
    let follow = wait_command(&[receipt.run_id], Some(&request.commit), true, true, None);
    if !options.json {
        output::success(&format!(
            "Authorized release run #{} — {} at {}@{} to {}",
            receipt.run_id,
            display(preview.workflow_label()),
            display(&request.branch),
            short(&request.commit),
            display(&request.environment),
        ));
        output::print_line(&run_url);
    }

    let mut code = 0;
    let mut wait = None;
    let mut wait_error = None;
    if let Some(minutes) = options.wait_minutes {
        let wait_options = WaitOptions {
            timeout: std::time::Duration::from_secs(minutes.saturating_mul(60)),
            json: options.json,
            // The embedded document uses the bounded, log-free projection,
            // which needs a full-hash fence: `request.commit` is one.
            summary: options.json,
            progress: if options.json {
                ProgressMode::Off
            } else {
                ProgressMode::Text
            },
        };
        if !options.json {
            output::info(&format!(
                "Waiting up to {minutes} min for run #{} to conclude…",
                receipt.run_id
            ));
        }
        let started = tokio::time::Instant::now();
        let mut progress = ProgressTracker::new(wait_options.progress, started);
        let waited = wait_exact(
            path,
            &api,
            &[receipt.run_id],
            Some(&request.commit),
            wait_options,
            started,
            &mut progress,
            None,
            WaitDelivery::Return,
        )
        .await;
        match waited {
            Ok((wait_code, document)) => {
                code = wait_code;
                wait = document;
            }
            // Text mode: the run id and URL are already printed; report the
            // error the usual way.
            Err(error) if !options.json => return Err(error),
            // JSON mode: still deliver the receipt in the one document. Wait
            // errors are server/network failures (inputs were validated),
            // which the CLI's exit-code contract maps to 6.
            Err(error) => {
                code = 6;
                wait_error = Some(error.to_string());
            }
        }
    } else if !options.json {
        output::info(&format!(
            "Follow it with: {}",
            wait_command(&[receipt.run_id], Some(&request.commit), false, false, None)
        ));
    }

    if options.json {
        let recommended = match wait
            .as_ref()
            .and_then(|doc| doc.get("recommended_next_commands"))
        {
            Some(commands) => serde_json::from_value(commands.clone()).unwrap_or_default(),
            None => vec![follow],
        };
        output::print_json(&DeployJson {
            schema_version: SCHEMA_VERSION,
            outcome: Outcome::Authorized.as_str(),
            preview: &preview,
            authorization: Some(AuthorizationJson {
                authorization_id: &receipt.authorization_id,
                run_id: receipt.run_id,
                run_url,
                request: &request,
            }),
            wait,
            wait_error,
            recommended_next_commands: recommended,
        })?;
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview() -> ReleasePreview {
        ReleasePreview {
            environment: "production".into(),
            branch: "main".into(),
            commit: "a".repeat(64),
            workflow: "release-macos".into(),
            workflow_path: ".oak/workflows/release-macos.yml".into(),
            workflow_hash: "b".repeat(64),
            event: "manual".into(),
            policy_version: 7,
            required_reviewers: true,
            reviewers: vec!["zach".into()],
            runner_online: true,
            can_authorize: true,
            ready: true,
            problems: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn authorization_request_copies_the_preview_verbatim() {
        let request = preview()
            .authorization_request("production", Some("release-macos"), Some("main"))
            .unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "environment": "production",
                "branch": "main",
                "commit": "a".repeat(64),
                "workflow_path": ".oak/workflows/release-macos.yml",
                "workflow_hash": "b".repeat(64),
                "event": "manual",
                "policy_version": 7,
            })
        );
        // A workflow may be named by path too, like the server accepts.
        assert!(preview()
            .authorization_request("production", Some(".oak/workflows/release-macos.yml"), None)
            .is_ok());
    }

    #[test]
    fn authorization_request_rejects_malformed_or_unrelated_previews() {
        type Mutation = fn(&mut ReleasePreview);
        let cases: [(&str, Mutation); 7] = [
            ("short commit", |p| p.commit = "a".repeat(12)),
            ("uppercase commit", |p| p.commit = "A".repeat(64)),
            ("missing hash", |p| p.workflow_hash.clear()),
            ("zero policy", |p| p.policy_version = 0),
            ("missing path", |p| p.workflow_path.clear()),
            ("control chars", |p| p.branch = "ma\x1bin".into()),
            ("other env", |p| p.environment = "staging".into()),
        ];
        for (name, mutate) in cases {
            let mut p = preview();
            mutate(&mut p);
            assert!(
                p.authorization_request("production", None, None).is_err(),
                "{name}"
            );
        }
        assert!(preview()
            .authorization_request("production", None, Some("release"))
            .is_err());
        assert!(preview()
            .authorization_request("production", Some("deploy"), None)
            .is_err());
    }

    #[test]
    fn missing_permission_fields_never_read_as_permission() {
        let parsed: ReleasePreview =
            serde_json::from_str(r#"{"environment":"production"}"#).unwrap();
        assert!(!parsed.ready && !parsed.can_authorize);
        assert_eq!(blocker(&parsed), Some(Outcome::NotReady));
    }

    #[test]
    fn readiness_is_checked_before_the_caller() {
        let mut p = preview();
        assert_eq!(blocker(&p), None);
        p.can_authorize = false;
        assert_eq!(blocker(&p), Some(Outcome::CannotAuthorize));
        p.ready = false;
        assert_eq!(blocker(&p), Some(Outcome::NotReady));
        assert_eq!(Outcome::NotReady.exit_code(), 1);
        assert_eq!(Outcome::CannotAuthorize.exit_code(), 6);
        assert_eq!(Outcome::ConfirmationRequired.exit_code(), 2);
    }

    #[test]
    fn recommended_deploy_commands_pin_the_preview_and_quote_values() {
        let mut p = preview();
        assert_eq!(
            deploy_command(&p, " --yes --json"),
            "oak ci deploy -e production --workflow .oak/workflows/release-macos.yml --branch main --yes --json"
        );
        p.branch = "feature x".into();
        assert!(deploy_command(&p, "").ends_with("--branch 'feature x'"));
    }

    #[test]
    fn options_validate_before_any_request() {
        let ok = DeployOptions {
            environment: "production".into(),
            ..DeployOptions::default()
        };
        assert!(validate_options(&ok).is_ok());
        for bad in [
            DeployOptions {
                environment: String::new(),
                ..ok.clone()
            },
            DeployOptions {
                environment: "a/b".into(),
                ..ok.clone()
            },
            DeployOptions {
                branch: Some("x\ny".into()),
                ..ok.clone()
            },
            DeployOptions {
                dry_run: true,
                wait_minutes: Some(5),
                ..ok.clone()
            },
        ] {
            assert!(matches!(
                validate_options(&bad),
                Err(OakError::InvalidArgument(_))
            ));
        }
    }

    #[test]
    fn cannot_authorize_message_names_who_can() {
        let mut p = preview();
        p.can_authorize = false;
        assert!(cannot_authorize_message(&p).contains("only its reviewers can: zach"));
        p.reviewers.clear();
        assert!(cannot_authorize_message(&p).contains("has none configured"));
        p.required_reviewers = false;
        assert!(cannot_authorize_message(&p).contains("write access"));
    }
}
