//! `oak queue`: put a branch in its repo's merge queue instead of merging it.
//!
//! The queue lives on the server (`/api/:owner/:name/queue`, the server's
//! `merge-queue` flag, enabled per repo). It tests each queued branch together
//! with everything ahead of it, as the exact tree `main` will have, and lands
//! branches in order, so `main` only ever lands a tested tree. While a repo's
//! queue requires it, `oak merge` onto `main` is refused with
//! `merge_queue_required`.
//!
//! Targets resolve like `oak findings`: the checkout's remote, repo and
//! current branch, or `--repo OWNER/NAME --branch NAME` from anywhere.

use std::path::Path;
use std::time::{Duration, Instant};

use oak_core::{OakError, Result};
use serde_json::{json, Value};

use super::findings::{bounded_body, shell_word, Target, TargetArgs};
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

fn queue_url(target: &Target) -> String {
    format!(
        "{}/api/{}/{}/queue",
        target.remote,
        urlencoding::encode(&target.owner),
        urlencoding::encode(&target.repo),
    )
}

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
        .and_then(Value::as_str);
    if status == reqwest::StatusCode::NOT_FOUND
        && matches!(message.as_deref(), None | Some("Not found"))
    {
        return OakError::Server(format!(
            "No merge queue for {}/{}: the merge queue isn't available for this repository, or \
             it isn't visible to you (HTTP 404)",
            target.owner, target.repo
        ));
    }
    let hint = match code {
        Some("branch_moved") => " — push the branch (`oak push`) so the queue tests what you have",
        Some("queue_disabled") => " — use `oak merge`",
        Some("approvals_required") => " — see `oak owners --branch --json`",
        _ => "",
    };
    let detail = match (message, code) {
        (Some(message), Some(code)) => format!("{message} [{code}]{hint}"),
        (Some(message), None) => message,
        (None, _) => "no details".to_string(),
    };
    OakError::Server(format!(
        "Merge queue request refused (HTTP {status}): {detail}"
    ))
}

async fn send(target: &Target, req: reqwest::RequestBuilder) -> Result<Value> {
    let response = target
        .authed(req)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("merge queue request failed: {e}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        // The same typed refusal and exit code as `oak merge`.
        if let Some(refused) = super::approve::merge_refusal(&target.branch, &body) {
            return Err(refused);
        }
        return Err(refusal(target, status, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| OakError::Server("invalid merge queue response".to_string()))?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(u64::from(SCHEMA_VERSION)) {
        return Err(OakError::Server(
            "unsupported merge queue response schema; upgrade the oak CLI".to_string(),
        ));
    }
    Ok(value)
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn branch_flags(target: &Target, explicit: bool) -> String {
    if explicit {
        format!(
            " --repo {}/{} --branch {}",
            target.owner,
            target.repo,
            shell_word(&target.branch)
        )
    } else {
        format!(" {}", shell_word(&target.branch))
    }
}

/// `oak queue add [BRANCH]`.
pub async fn add(
    path: &Path,
    args: &TargetArgs,
    priority: i32,
    wait: Option<u64>,
    json: bool,
) -> Result<()> {
    let target = Target::resolve(path, args)?;
    // From a checkout, queue exactly the head this checkout has: if it
    // isn't pushed, the server refuses with `branch_moved` instead of
    // queueing a head nobody here has seen.
    let mut body = json!({"branch": target.branch, "priority": priority});
    if let Some(head) = target.local_head.as_deref() {
        body["expected_head"] = json!(head);
    }
    let value = send(
        &target,
        crate::http::api_client()
            .post(queue_url(&target))
            .json(&body),
    )
    .await?;
    let entry = value.get("entry").cloned().unwrap_or(Value::Null);
    if let Some(minutes) = wait {
        return wait_for(&target, args.repo.is_some(), minutes, json).await;
    }
    if json {
        return output::print_json(&value);
    }
    let created = value.get("created").and_then(Value::as_bool) == Some(true);
    let position = entry.get("position").and_then(Value::as_u64).unwrap_or(0);
    output::print_line(&format!(
        "{} '{}' at {} — position {position} of {} in {}/{}'s merge queue",
        if created { "Queued" } else { "Already queued:" },
        target.branch,
        short(str_field(&entry, "head")),
        value
            .get("queue_length")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        target.owner,
        target.repo,
    ));
    output::print_line(&format!(
        "  Follow it with `oak queue status{}`, or wait with `oak queue add --wait`.",
        if args.repo.is_some() {
            branch_flags(&target, true)
        } else {
            String::new()
        }
    ));
    Ok(())
}

/// Poll until `target.branch` leaves the queue; Ok when it landed.
async fn wait_for(target: &Target, explicit: bool, minutes: u64, json: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(minutes.max(1) * 60);
    let activity = output::activity(format!(
        "Waiting for '{}' in the merge queue",
        target.branch
    ));
    loop {
        let queue = send(target, crate::http::api_client().get(queue_url(target))).await?;
        let find = |key: &str| {
            queue.get(key).and_then(Value::as_array).and_then(|items| {
                items
                    .iter()
                    .find(|e| str_field(e, "branch") == target.branch)
                    .cloned()
            })
        };
        if let Some(active) = find("entries") {
            activity.set_message(format!(
                "'{}' is {} (position {})",
                target.branch,
                str_field(&active, "state"),
                active.get("position").and_then(Value::as_u64).unwrap_or(0)
            ));
        } else if let Some(done) = find("recent") {
            activity.finish_and_clear();
            let state = str_field(&done, "state").to_string();
            if json {
                output::print_json(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "entry": done,
                    "landed": state == "landed",
                }))?;
            } else if state == "landed" {
                output::print_line(&format!(
                    "Landed '{}' on main as {}",
                    target.branch,
                    short(str_field(&done, "landed_commit"))
                ));
            }
            if state == "landed" {
                return Ok(());
            }
            return Err(OakError::Server(format!(
                "'{}' left the merge queue without landing: {} ({}){}",
                target.branch,
                state,
                str_field(&done, "detail"),
                match str_field(&done, "failure_reason") {
                    "ci_failed" => format!(
                        ". Read the failure with `oak ci status`, push a fix, then `oak queue add{}`",
                        branch_flags(target, explicit)
                    ),
                    "conflict" => format!(
                        ". Run `oak pull` to resolve it on the branch, push, then `oak queue add{}`",
                        branch_flags(target, explicit)
                    ),
                    _ => String::new(),
                }
            )));
        } else {
            activity.finish_and_clear();
            return Err(OakError::Server(format!(
                "'{}' isn't in the merge queue",
                target.branch
            )));
        }
        if Instant::now() >= deadline {
            activity.finish_and_clear();
            return Err(OakError::Server(format!(
                "Timed out after {minutes} min waiting for '{}'; it is still queued",
                target.branch
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `oak queue status`.
pub async fn status(path: &Path, args: &TargetArgs, json: bool) -> Result<()> {
    let target = Target::resolve(path, args)?;
    let mut value = send(&target, crate::http::api_client().get(queue_url(&target))).await?;
    let mine_active = value
        .get("entries")
        .and_then(Value::as_array)
        .is_some_and(|e| e.iter().any(|e| str_field(e, "branch") == target.branch));
    if json {
        value["branch"] = json!(target.branch);
        value["recommended_next_commands"] = if mine_active {
            json!([format!(
                "oak queue add --wait{}",
                branch_flags(&target, args.repo.is_some())
            )])
        } else {
            json!([])
        };
        return output::print_json(&value);
    }
    let config = value.get("config").cloned().unwrap_or(Value::Null);
    let enabled = config.get("enabled").and_then(Value::as_bool) == Some(true);
    output::print_line(&format!(
        "{}/{} merge queue: {}{}",
        target.owner,
        target.repo,
        if enabled { "on" } else { "off" },
        if enabled && config.get("require_queue").and_then(Value::as_bool) == Some(true) {
            " (required for main)"
        } else {
            ""
        }
    ));
    if let Some(stats) = value.get("stats") {
        output::print_line(&format!(
            "  batch target {} · est. failure rate {:.1}% · {} of the last {} trains failed",
            stats
                .get("target_batch")
                .and_then(Value::as_u64)
                .unwrap_or(1),
            stats
                .get("estimated_failure_rate")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                * 100.0,
            stats
                .get("recent_failed_trains")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            stats
                .get("recent_trains")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ));
    }
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        output::print_line("  (nothing queued)");
    }
    for entry in &entries {
        let marker = if str_field(entry, "branch") == target.branch {
            "*"
        } else {
            " "
        };
        let train = entry
            .get("train")
            .and_then(Value::as_i64)
            .map(|t| format!(" train #{t}"))
            .unwrap_or_default();
        output::print_line(&format!(
            "{marker} {:>3}. {:<32} {:<8} {}{train}",
            entry.get("position").and_then(Value::as_u64).unwrap_or(0),
            str_field(entry, "branch"),
            str_field(entry, "state"),
            short(str_field(entry, "head")),
        ));
    }
    let live: Vec<&Value> = value
        .get("trains")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| matches!(str_field(t, "state"), "testing" | "passed" | "failed"))
        .collect();
    if !live.is_empty() {
        output::print_line("Trains:");
        for train in live {
            let branches: Vec<&str> = train
                .get("branches")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            output::print_line(&format!(
                "  #{} {:<8} {}{}",
                train.get("id").and_then(Value::as_i64).unwrap_or(0),
                str_field(train, "state"),
                branches.join(" + "),
                train
                    .get("stacked_on")
                    .and_then(Value::as_i64)
                    .map(|p| format!(" (on #{p})"))
                    .unwrap_or_default(),
            ));
        }
    }
    let recent = value
        .get("recent")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !recent.is_empty() {
        output::print_line("Recent:");
        for entry in recent.iter().take(5) {
            output::print_line(&format!(
                "  {:<32} {:<9} {}",
                str_field(entry, "branch"),
                str_field(entry, "state"),
                match str_field(entry, "state") {
                    "landed" => short(str_field(entry, "landed_commit")).to_string(),
                    _ => str_field(entry, "detail").chars().take(100).collect(),
                }
            ));
        }
    }
    Ok(())
}

/// `oak queue remove [BRANCH]`.
pub async fn remove(path: &Path, args: &TargetArgs, json: bool) -> Result<()> {
    let target = Target::resolve(path, args)?;
    let url = format!(
        "{}/entries/{}",
        queue_url(&target),
        super::branch_api_segment(&target.branch)
    );
    let value = send(&target, crate::http::api_client().delete(url)).await?;
    if json {
        return output::print_json(&value);
    }
    if value.get("removed").and_then(Value::as_bool) == Some(true) {
        output::print_line(&format!("Removed '{}' from the merge queue", target.branch));
    } else {
        output::print_line(&format!("'{}' wasn't in the merge queue", target.branch));
    }
    Ok(())
}
