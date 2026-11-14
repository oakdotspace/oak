//! `oak approve`: an owner's approval of a branch's changes inside their
//! ownership boundary, the merge gate of a repo with boundaries
//! (`/api/:owner/:name/branches/:branch/approvals`).
//!
//! An approval has to come from a person. The branch page is one way; the
//! other is an API key that explicitly lists the `review` scope. The server
//! refuses the credentials an agent normally holds (`oak login` tokens,
//! unscoped keys, service accounts), and this command says what to do instead.
//!
//! Targets resolve through `branch_target`: the checkout's remote, repo and
//! branch, or `--repo OWNER/NAME --branch NAME` from anywhere. From a checkout
//! the approval names the head this checkout has, so a branch that moved since
//! is refused rather than approved unseen.

use std::path::Path;
use std::time::Duration;

use oak_core::{MissingApproval, OakError, Result};
use serde_json::{json, Value};

use super::branch_target::{bounded_body, Target, TargetArgs};
use crate::output;

/// The approvals response schema this CLI reads (version 2: boundaries).
pub const SCHEMA_VERSION: u32 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Most missing approvals read from a server response.
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
                path: m
                    .get("path")?
                    .as_str()?
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_FIELD_CHARS)
                    .collect(),
                owner_team: m.get("owner_team").and_then(clean).unwrap_or_default(),
                approvers: m
                    .get("approvers")
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
/// `approvals_required`, or `None` for any other (CI, a proxy).
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
        .map(|m| match m.approvers.as_slice() {
            [] => format!(
                "{} (no one can approve: only an org admin can force it)",
                display_path(&m.path)
            ),
            who => format!("{} ({})", display_path(&m.path), who.join(", ")),
        })
        .collect();
    if hidden > 0 {
        needs.push(format!("{hidden} boundary(ies) you can't see"));
    }
    let needs = if needs.is_empty() {
        "the boundaries it touches".to_string()
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
        code: oak_core::ErrorCode::ApprovalsRequired,
        server_code: None,
        message: format!(
            "Merge refused: branch '{branch}' needs approval from the owners of: {needs}. \
             Waiting won't clear this: ask an owner who didn't work on the branch to approve \
             it, on the branch page or with `oak approve {branch}`.{force_note}"
        ),
        missing,
        hidden,
        recommended_next_commands: vec!["oak owners --branch --json".to_string()],
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
            "No approvals for {}/{}: path rules aren't on for this repository, or it isn't visible \
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
    // The server's reason is parsed into the closed code set here, where it
    // is received. Every reason the server's table emits keeps its exact
    // string; one this client does not know is `approval_refused`, with the
    // raw value kept as `server_code`.
    use oak_core::ErrorCode;
    let (code, server_code, hint, next) = match code {
        Some("review_key_required") | None if status == reqwest::StatusCode::FORBIDDEN => (
            ErrorCode::ReviewKeyRequired,
            None,
            format!("\nBranch page: {branch_page}"),
            vec![format!("oak open --print")],
        ),
        Some("branch_moved") => (
            ErrorCode::BranchMoved,
            None,
            " — `oak pull`, review the new head and approve again".to_string(),
            vec!["oak pull".to_string()],
        ),
        Some(other) => match ErrorCode::from_approval_reason(other) {
            Some(code) => (code, None, String::new(), Vec::new()),
            None => (
                ErrorCode::ApprovalRefused,
                Some(other.to_string()),
                String::new(),
                Vec::new(),
            ),
        },
        None => (ErrorCode::ApprovalRefused, None, String::new(), Vec::new()),
    };
    OakError::ApprovalRefused {
        code,
        server_code,
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
    boundary: Option<&str>,
    head: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let target = Target::resolve(path, args)?;
    let mut body = json!({});
    if let Some(boundary) = boundary {
        body["path"] = json!(boundary.replace('\\', "/").trim_matches('/'));
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
        .get("boundaries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = row.get("path").and_then(Value::as_str).unwrap_or_default();
        if row.get("approved").and_then(Value::as_bool) == Some(true) {
            output::print_line(&format!("✓ {}", display_path(path)));
        }
    }
    let missing = parse_missing(&value);
    if missing.is_empty() {
        output::success(&format!("'{}' has every approval it needs", target.branch));
    }
    for m in &missing {
        output::print_line(&format!(
            "  still needs {} ({})",
            display_path(&m.path),
            m.approvers.join(", ")
        ));
    }
    Ok(())
}

/// A boundary's directory as people read it: `/` for the repo root.
fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        format!("{path}/")
    }
}
