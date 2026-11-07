//! `oak approve`: a maintainer's approval of a branch's slice of their
//! project, the merge gate of a company repo (`/api/:owner/:name/branches/
//! :branch/approvals`).
//!
//! An approval has to come from a person. The branch page is one way; the
//! other is an API key that explicitly lists the `review` scope. The server
//! refuses the credentials an agent normally holds (`oak login` tokens,
//! unscoped keys, service accounts), and this command says what to do instead.
//!
//! Targets resolve like `oak findings`: the checkout's remote, repo and
//! branch, or `--repo OWNER/NAME --branch NAME` from anywhere. From a checkout
//! the approval names the head this checkout has, so a branch that moved since
//! is refused rather than approved unseen.

use std::path::Path;
use std::time::Duration;

use oak_core::{MissingApproval, OakError, Result};
use serde_json::{json, Value};

use super::findings::{bounded_body, Target, TargetArgs};
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Most missing projects read from a server response.
const MAX_MISSING: usize = 200;
const MAX_FIELD_CHARS: usize = 200;

fn approvals_url(target: &Target) -> String {
    format!(
        "{}/api/{}/{}/branches/{}/approvals",
        target.remote,
        urlencoding::encode(&target.owner),
        urlencoding::encode(&target.repo),
        super::branch_api_segment(&target.branch),
    )
}

fn clean(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty()).then(|| {
        text.chars()
            .filter(|c| !c.is_control())
            .take(MAX_FIELD_CHARS)
            .collect()
    })
}

/// The structured `missing_approvals` of a refusal or status. Only these
/// fields are read, bounded and stripped of control characters.
pub fn parse_missing(value: &Value) -> Vec<MissingApproval> {
    value
        .get("missing_approvals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_MISSING)
        .filter_map(|m| {
            Some(MissingApproval {
                project: clean(m.get("project")?)?,
                path: m.get("path").and_then(clean).unwrap_or_default(),
                maintainers: m
                    .get("maintainers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(MAX_MISSING)
                    .filter_map(clean)
                    .collect(),
            })
        })
        .collect()
}

/// The `oak merge` refusal for a 409 (a 412 from an older server) typed
/// `approvals_required`, or `None` for any other (CI, findings, a proxy).
/// Waiting never clears this one.
pub fn merge_refusal(branch: &str, body: &[u8]) -> Option<OakError> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.get("code").and_then(Value::as_str) != Some("approvals_required") {
        return None;
    }
    let missing = parse_missing(&value);
    let hidden = value
        .get("hidden_missing_approvals")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let mut needs: Vec<String> = missing
        .iter()
        .map(|m| match m.maintainers.as_slice() {
            [] => format!(
                "{} (no maintainers: only an org admin can force it)",
                m.project
            ),
            who => format!("{} ({})", m.project, who.join(", ")),
        })
        .collect();
    if hidden > 0 {
        needs.push(format!("{hidden} project(s) you can't read"));
    }
    let needs = if needs.is_empty() {
        "the projects it touches".to_string()
    } else {
        needs.join("; ")
    };
    // Only this typed value is read, and only known values are rendered.
    let force_note = match value.get("force_policy").and_then(Value::as_str) {
        Some("browser") => {
            " `--force` didn't get past this: in this repo an org admin forces approvals on the \
             branch page."
        }
        Some("nobody") => {
            " `--force` didn't get past this: this repo doesn't let anyone force approvals."
        }
        Some("any_credential") => {
            " `--force` didn't get past this: only an org admin can force approvals."
        }
        _ => "",
    };
    Some(OakError::ApprovalRefused {
        code: "approvals_required".to_string(),
        message: format!(
            "Merge refused: branch '{branch}' needs maintainer approval: {needs}. Waiting won't \
             clear this: ask a maintainer who didn't work on the branch to approve it, on the \
             branch page or with `oak approve {branch}`.{force_note}"
        ),
        missing,
        hidden,
        recommended_next_commands: vec!["oak project impact --json".to_string()],
    })
}

fn refusal(target: &Target, status: reqwest::StatusCode, body: &[u8]) -> OakError {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return OakError::AuthRequired {
            remote: target.remote.clone(),
            status: status.as_u16(),
        };
    }
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let message = parsed.as_ref().and_then(|v| v.get("error")).and_then(clean);
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str);
    if status == reqwest::StatusCode::NOT_FOUND
        && matches!(message.as_deref(), None | Some("Not found"))
    {
        return OakError::Server(format!(
            "No approvals for {}/{}: projects aren't on for this repository, or it isn't visible \
             to you (HTTP 404)",
            target.owner, target.repo
        ));
    }
    let branch_page = super::branch_web_url(
        &target.remote,
        &format!("{}/{}", target.owner, target.repo),
        &target.branch,
    );
    // The capability check answers before the handler, with no `code`.
    let (code, hint, next) = match code {
        Some("review_key_required") | None if status == reqwest::StatusCode::FORBIDDEN => (
            "review_key_required".to_string(),
            format!("\nBranch page: {branch_page}"),
            vec![format!("oak open --print")],
        ),
        Some("branch_moved") => (
            "branch_moved".to_string(),
            " — `oak pull`, review the new head and approve again".to_string(),
            vec!["oak pull".to_string()],
        ),
        Some(other) => (other.to_string(), String::new(), Vec::new()),
        None => ("approval_refused".to_string(), String::new(), Vec::new()),
    };
    OakError::ApprovalRefused {
        code,
        message: format!(
            "{}{hint}",
            message.unwrap_or_else(|| format!("Approval refused (HTTP {status})"))
        ),
        missing: Vec::new(),
        hidden: 0,
        recommended_next_commands: next,
    }
}

pub async fn run(
    path: &Path,
    args: &TargetArgs,
    project: Option<&str>,
    head: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let target = Target::resolve(path, args)?;
    let mut body = json!({});
    if let Some(project) = project {
        body["project"] = json!(project);
    }
    if let Some(head) = head.or(target.local_head.as_deref()) {
        body["expected_head"] = json!(head);
    }
    let response = target
        .authed(crate::http::api_client().post(approvals_url(&target)))
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("approval request failed: {e}")))?;
    let status = response.status();
    let bytes = bounded_body(response).await?;
    if !status.is_success() {
        return Err(refusal(&target, status, &bytes));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| OakError::Server("invalid approval response".to_string()))?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(u64::from(SCHEMA_VERSION)) {
        return Err(OakError::Server(
            "unsupported approval response schema; upgrade the oak CLI".to_string(),
        ));
    }
    if json_output {
        return output::print_json(&value);
    }
    for row in value
        .get("projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = row.get("project").and_then(clean).unwrap_or_default();
        if row.get("approved").and_then(Value::as_bool) == Some(true) {
            output::print_line(&format!("✓ {name}"));
        }
    }
    let missing = parse_missing(&value);
    if missing.is_empty() {
        output::success(&format!("'{}' has every approval it needs", target.branch));
    }
    for m in &missing {
        output::print_line(&format!(
            "  still needs {} ({})",
            m.project,
            m.maintainers.join(", ")
        ));
    }
    Ok(())
}
