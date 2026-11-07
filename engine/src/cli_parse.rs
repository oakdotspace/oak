//! `cli.parse`: what an `oak` run meant. Every mapping here was read off real
//! CLI output (`engine/fixtures/capture/capture.sh`) or, where `oak serve` can't
//! produce a case, the CLI source; `engine/fixtures/conformance/` replays them.
//!
//! The order is: a JSON `error` envelope first (it carries Oak's own code),
//! then the handful of commands that report a state through a non-zero exit
//! alongside a normal document, then the exit code, then text only where an
//! exit code covers several causes. Exit 6 is the main such case: it means
//! "network, server or auth", and nothing but the message tells them apart.

use serde_json::{json, Map, Value};

use crate::args::Args;
use crate::cli_plan::ACTIONS;
use crate::error::EngineError;
use crate::version::parse_cli_version;

/// The first JSON document in `text`, tolerating noise before it. Absent if
/// there's no parseable document.
fn lenient_json(text: &str) -> Option<Value> {
    let start = text.find(['{', '['])?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn string_field(value: &Value, key: &str) -> Value {
    value
        .get(key)
        .filter(|field| field.is_string())
        .cloned()
        .unwrap_or(Value::Null)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn int_field(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn array_field(value: &Value, key: &str) -> Value {
    value
        .get(key)
        .filter(|field| field.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]))
}

/// Oak's JSON rule, applied field by field: unknown fields are dropped and an
/// absent one becomes its default.
#[derive(Clone, Copy)]
enum Field {
    Str(&'static str),
    Bool(&'static str),
    Int(&'static str),
    List(&'static str),
    Any(&'static str),
}

fn pick(document: &Value, fields: &[Field]) -> Map<String, Value> {
    fields
        .iter()
        .map(|field| match *field {
            Field::Str(key) => (key.to_string(), string_field(document, key)),
            Field::Bool(key) => (key.to_string(), bool_field(document, key).into()),
            Field::Int(key) => (key.to_string(), int_field(document, key).into()),
            Field::List(key) => (key.to_string(), array_field(document, key)),
            Field::Any(key) => (
                key.to_string(),
                document.get(key).cloned().unwrap_or(Value::Null),
            ),
        })
        .collect()
}

use Field::{Any, Bool, Int, List, Str};

const REPO_FIELDS: &[Field] = &[
    Str("branch"),
    Str("branch_description"),
    Str("parent"),
    Str("head"),
    Str("branch_status"),
    Str("repo_owner"),
    Str("repo_name"),
    Str("remote_url"),
    Str("repository_root"),
    Str("web_url"),
    Str("review_url"),
    Bool("merge_in_progress"),
    Bool("sync_in_progress"),
];

fn change(change: &Value) -> Value {
    json!({
        "path": string_field(change, "path"),
        "status": string_field(change, "status"),
        "old_path": string_field(change, "old_path"),
    })
}

fn changes(list: &Value) -> Vec<Value> {
    list.as_array()
        .map_or_else(Vec::new, |items| items.iter().map(change).collect())
}

fn status_result(document: &Value) -> Value {
    let mut result = pick(document, REPO_FIELDS);
    result.extend(pick(document, &[Int("unmerged_commit_count")]));
    result.insert(
        "changes".into(),
        changes(&array_field(document, "changes")).into(),
    );
    let branch_changes = document
        .get("branch_changes")
        .map_or_else(Vec::new, |block| changes(&array_field(block, "changes")));
    result.insert("branch_changes".into(), branch_changes.into());
    let progress = document
        .get("progress_state")
        .cloned()
        .unwrap_or_else(|| json!({}));
    result.insert(
        "conflict_paths".into(),
        array_field(&progress, "conflict_paths"),
    );
    result.insert(
        "next_commands".into(),
        array_field(&progress, "next_commands"),
    );
    result.into()
}

/// Structured hunks from a unified `patch`. A hunk header that doesn't parse
/// ends the current hunk, and lines outside a hunk (the file header) are
/// skipped, so an unexpected patch shape loses detail and never misnumbers.
fn parse_patch(patch: &str) -> Vec<Value> {
    let mut hunks: Vec<Value> = Vec::new();
    let mut current: Option<(Map<String, Value>, Vec<Value>)> = None;
    let (mut old_line, mut new_line) = (0_u64, 0_u64);

    let mut finish = |current: &mut Option<(Map<String, Value>, Vec<Value>)>| {
        if let Some((mut hunk, lines)) = current.take() {
            hunk.insert("lines".into(), lines.into());
            hunks.push(hunk.into());
        }
    };

    for line in patch.lines() {
        if line.starts_with("@@") {
            finish(&mut current);
            if let Some((old_start, old_count, new_start, new_count)) = hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
                let mut hunk = Map::new();
                hunk.insert("old_start".into(), old_start.into());
                hunk.insert("old_count".into(), old_count.into());
                hunk.insert("new_start".into(), new_start.into());
                hunk.insert("new_count".into(), new_count.into());
                hunk.insert("header".into(), line.into());
                current = Some((hunk, Vec::new()));
            }
            continue;
        }
        let Some((_, lines)) = current.as_mut() else {
            continue;
        };
        let (marker, text) = line.split_at(line.len().min(1));
        match marker {
            "+" => {
                lines.push(
                    json!({"kind": "added", "text": text, "old_line": null, "new_line": new_line}),
                );
                new_line += 1;
            }
            "-" => {
                lines.push(json!({"kind": "removed", "text": text, "old_line": old_line, "new_line": null}));
                old_line += 1;
            }
            " " => {
                lines.push(json!({"kind": "context", "text": text, "old_line": old_line, "new_line": new_line}));
                old_line += 1;
                new_line += 1;
            }
            "\\" => {
                if let Some(Value::Object(last)) = lines.last_mut() {
                    last.insert("no_newline".into(), true.into());
                }
            }
            _ => {}
        }
    }
    finish(&mut current);
    hunks
}

/// `@@ -old_start[,old_count] +new_start[,new_count] @@ ...`
fn hunk_header(line: &str) -> Option<(u64, u64, u64, u64)> {
    let mut parts = line.split(' ');
    let (_, old, new) = (parts.next()?, parts.next()?, parts.next()?);
    let range = |text: &str| -> Option<(u64, u64)> {
        Some(match text.split_once(',') {
            Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
            None => (text.parse().ok()?, 1),
        })
    };
    let (old_start, old_count) = range(old.strip_prefix('-')?)?;
    let (new_start, new_count) = range(new.strip_prefix('+')?)?;
    Some((old_start, old_count, new_start, new_count))
}

fn diff_result(document: &Value) -> Value {
    let files: Vec<Value> = array_field(document, "changed_files")
        .as_array()
        .map_or_else(Vec::new, |files| {
            files
                .iter()
                .map(|file| {
                    let patch = file.get("patch").and_then(Value::as_str).unwrap_or("");
                    let mut entry = pick(
                        file,
                        &[
                            Str("path"),
                            Str("status"),
                            Str("old_path"),
                            Int("additions"),
                            Int("deletions"),
                            Bool("patch_omitted"),
                            Str("patch_omitted_reason"),
                        ],
                    );
                    entry.insert(
                        "binary".into(),
                        (bool_field(file, "binary_or_large") || patch.contains("\nBinary files "))
                            .into(),
                    );
                    entry.insert("hunks".into(), parse_patch(patch).into());
                    entry.into()
                })
                .collect()
        });
    let page = document.get("changed_files_page");
    json!({
        "files": files,
        "changed_file_count": int_field(document, "changed_file_count"),
        "truncated": bool_field(document, "hunks_truncated"),
        "next_offset": page.and_then(|page| page.get("next_offset")).cloned().unwrap_or(Value::Null),
    })
}

fn log_result(document: &Value) -> Value {
    let commits: Vec<Value> = document.as_array().map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .map(|entry| {
                json!({
                    "hash": string_field(entry, "hash"),
                    "timestamp": string_field(entry, "timestamp"),
                    "branch": string_field(entry, "branch"),
                    "description": string_field(entry, "description_or_subject"),
                    "files_changed": int_field(entry, "files_changed"),
                })
            })
            .collect()
    });
    json!({ "commits": commits })
}

fn branch_list_result(document: &Value) -> Value {
    let branches: Vec<Value> = document.as_array().map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .map(|entry| {
                pick(
                    entry,
                    &[
                        Str("name"),
                        Str("head"),
                        Str("description"),
                        Str("status"),
                        Str("created_at"),
                        Bool("current"),
                    ],
                )
                .into()
            })
            .collect()
    });
    json!({ "branches": branches })
}

fn whoami_result(document: &Value, stdout: &str) -> Result<Value, EngineError> {
    let identity = document
        .get("identity")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let state = identity
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let detail = identity
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or(state);
    match state {
        "authenticated" => Ok(json!({
            "logged_in": true,
            "username": string_field(&identity, "username"),
            "display_name": string_field(&identity, "display_name"),
            "remote": string_field(document, "remote"),
        })),
        "unauthenticated" => Err(EngineError::new("not_logged_in", "Not logged in to Oak.")
            .detail("remote", string_field(document, "remote"))),
        "unreachable" => Err(
            EngineError::new("network", format!("Oak is unreachable: {detail}")).retryable(true),
        ),
        // A stored credential that couldn't be checked; `stdout` has the rest.
        _ => Err(
            EngineError::new("cli_failed", format!("Oak identity is {state}: {detail}"))
                .detail("stdout", stdout.trim().into()),
        ),
    }
}

const COMMIT_FIELDS: &[Field] = &[
    Bool("committed"),
    Str("branch"),
    Str("head_before"),
    Str("head_after"),
    Any("change_counts"),
    Int("unpushed_commit_count"),
];
const DESC_FIELDS: &[Field] = &[
    Str("branch"),
    Str("description"),
    Bool("local_saved"),
    Any("remote_synced"),
    Str("remote_state"),
];
const PUSH_FIELDS: &[Field] = &[
    Bool("pushed"),
    Bool("published"),
    Str("branch"),
    Str("pushed_head"),
    Str("review_url"),
];
const PULL_FIELDS: &[Field] = &[
    Str("branch"),
    Str("branch_after"),
    Str("head_before"),
    Str("head_after"),
    Str("parent_head_after"),
    Str("convergence"),
    Str("parent_sync"),
    Bool("parent_synced"),
    Int("commits_fetched"),
    List("conflict_paths"),
];
const MERGE_FIELDS: &[Field] = &[
    Bool("merged"),
    Str("branch"),
    Str("parent"),
    Str("new_branch"),
    Str("commit_hash"),
];
const CLOSE_FIELDS: &[Field] = &[Str("branch"), Str("status"), Str("current_branch")];
const CI_STATUS_FIELDS: &[Field] = &[
    Str("branch"),
    Str("commit"),
    Str("state"),
    Any("run"),
    Str("run_url"),
    Bool("checkout_matches_observed"),
];
const CONFLICT_FIELDS: &[Field] = &[
    Bool("in_progress"),
    Str("kind"),
    List("conflict_paths"),
    List("recommended_next_commands"),
];

fn message_result(stdout: &str) -> Value {
    json!({ "message": stdout.trim() })
}

/// True for clap's refusal of a subcommand or flag this CLI doesn't have.
fn is_usage_refusal(text: &str) -> bool {
    text.contains("unrecognized subcommand") || text.contains("unexpected argument")
}

/// The best human message: the envelope's, then stderr, then stdout. In
/// stderr, the last `error:` line and what follows it is the failure; anything
/// before it is progress output.
fn failure_message(
    action: &str,
    envelope_message: Option<&str>,
    stdout: &str,
    stderr: &str,
    exit: i64,
) -> String {
    let stderr = stderr.trim();
    let stderr = stderr
        .rfind("\nerror: ")
        .map_or(stderr, |at| &stderr[at + 1..]);
    let stderr = stderr.strip_prefix("error: ").unwrap_or(stderr);
    [envelope_message.unwrap_or(""), stderr, stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(
            || format!("oak {action} exited with code {exit}"),
            str::to_string,
        )
}

/// Exit 6 is one code for three causes, so this is the one place the mapping
/// reads text.
fn classify_exit_6(action: &str, oak_code: Option<&str>, message: &str) -> EngineError {
    let lower = message.to_ascii_lowercase();
    if oak_code == Some("auth_required") || lower.contains("not logged in") {
        return EngineError::new("not_logged_in", message);
    }
    if action == "merge"
        && (lower.contains("ci gates this merge")
            || lower.contains("http 412")
            || lower.contains("open important review finding")
            || lower.contains("approval"))
    {
        return EngineError::new("ci_blocked", message).retryable(true);
    }
    // `Server error:` is the server answering: retrying won't change it.
    EngineError::new("network", message).retryable(!lower.starts_with("server error"))
}

fn classify_failure(
    action: &str,
    exit: i64,
    oak_code: Option<&str>,
    message: String,
    usage_text: &str,
) -> EngineError {
    match oak_code {
        Some("auth_required") => return EngineError::new("not_logged_in", message),
        Some("auth_denied") => return EngineError::new("cli_failed", message),
        Some("repo_locked") => return EngineError::new("locked", message).retryable(true),
        Some("dirty_worktree") => return EngineError::new("dirty_tree", message),
        Some("conflict_detected" | "merge_in_progress") => {
            return EngineError::new("conflict", message)
        }
        _ => {}
    }
    match exit {
        2 if is_usage_refusal(usage_text) => EngineError::new("cli_too_old", message),
        3 => EngineError::new("locked", message).retryable(true),
        4 => EngineError::new("dirty_tree", message),
        5 => EngineError::new("conflict", message),
        6 => classify_exit_6(action, oak_code, &message),
        // 7: merge prediction uncertified. 8: integrity proof inconclusive.
        // Neither is a verdict, and both say to fetch and try again.
        7 | 8 => EngineError::new("cli_failed", message).retryable(true),
        _ => EngineError::new("cli_failed", message),
    }
}

fn envelope_failure(
    action: &str,
    exit: i64,
    error: &Value,
    stdout: &str,
    stderr: &str,
) -> EngineError {
    let oak_code = error.get("code").and_then(Value::as_str);
    let message = failure_message(
        action,
        error.get("message").and_then(Value::as_str),
        stdout,
        stderr,
        exit,
    );
    classify_failure(action, exit, oak_code, message, stderr)
        .detail("exit_code", exit.into())
        .detail("oak_code", string_field(error, "code"))
        .detail("conflict_paths", array_field(error, "conflict_paths"))
        .detail(
            "next_commands",
            array_field(error, "recommended_next_commands"),
        )
}

fn plain_failure(action: &str, exit: i64, stdout: &str, stderr: &str) -> EngineError {
    let message = failure_message(action, None, stdout, stderr, exit);
    classify_failure(action, exit, None, message, stderr)
        .detail("exit_code", exit.into())
        .detail("oak_code", Value::Null)
        .detail("conflict_paths", json!([]))
        .detail("next_commands", json!([]))
}

fn unparseable(action: &str, stdout: &str) -> EngineError {
    EngineError::new(
        "cli_failed",
        format!("oak {action} produced no parseable JSON"),
    )
    .detail("stdout", stdout.trim().into())
}

/// `ci publish`, preview or real: `outcome` says what happened, in the document
/// on every exit code. A preview that can't proceed is still a good preview.
fn publish_result(
    action: &str,
    exit: i64,
    document: &Value,
    expected_commit: Option<&str>,
) -> Result<Value, EngineError> {
    let outcome = document
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("");
    let preview = document
        .get("preview")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut result = pick(
        &preview,
        &[
            Str("environment"),
            Str("branch"),
            Str("commit"),
            Str("workflow"),
            Str("workflow_path"),
            Str("workflow_hash"),
            Any("policy_version"),
            Bool("required_reviewers"),
            List("reviewers"),
            Bool("runner_online"),
            Bool("can_authorize"),
            Bool("ready"),
            List("problems"),
            Bool("republish"),
        ],
    );
    result.insert("outcome".into(), outcome.into());
    let authorized = outcome == "authorized";
    if action == "ci_publish" && !authorized {
        let problems = array_field(&preview, "problems");
        return Err(
            EngineError::new("ci_blocked", format!("oak ci publish: {outcome}"))
                .detail("exit_code", exit.into())
                .detail("outcome", outcome.into())
                .detail("problems", problems),
        );
    }
    // The CLI re-previews before it authorizes, and a plain head publish can't
    // be fenced, so the branch can move after the preview the user approved.
    let commit = preview.get("commit").and_then(Value::as_str);
    if let (Some(expected), Some(commit)) = (expected_commit, commit) {
        result.insert("commit_moved".into(), (expected != commit).into());
    }
    if let Some(authorization) = document.get("authorization") {
        result.insert("run_id".into(), int_field(authorization, "run_id").into());
        result.insert("run_url".into(), string_field(authorization, "run_url"));
    }
    Ok(result.into())
}

fn normalize(action: &str, document: &Value, stdout: &str) -> Result<Value, EngineError> {
    Ok(match action {
        "status" => status_result(document),
        "info" => pick(document, REPO_FIELDS).into(),
        "desc_get" => json!({
            "branch": string_field(document, "branch"),
            "description": string_field(document, "branch_description"),
        }),
        "diff" => diff_result(document),
        "log" => log_result(document),
        "branch_list" => branch_list_result(document),
        "branch_close" => pick(document, CLOSE_FIELDS).into(),
        "commit" => pick(document, COMMIT_FIELDS).into(),
        "desc_append" => pick(document, DESC_FIELDS).into(),
        "pull" => pick(document, PULL_FIELDS).into(),
        "push" => pick(document, PUSH_FIELDS).into(),
        "merge" => pick(document, MERGE_FIELDS).into(),
        "whoami" => whoami_result(document, stdout)?,
        "ci_status" => pick(document, CI_STATUS_FIELDS).into(),
        "ci_runs" => json!({ "runs": array_field(document, "runs") }),
        // Runs and logs pass through: they're display data, and their shape
        // follows the server's.
        "ci_logs" => document.clone(),
        "conflict_status" => pick(document, CONFLICT_FIELDS).into(),
        other => return Err(EngineError::bad_request(format!("no parser for `{other}`"))),
    })
}

fn parse_inner(
    action: &str,
    exit: i64,
    stdout: &str,
    stderr: &str,
    expected_commit: Option<&str>,
) -> Result<Value, EngineError> {
    let document = lenient_json(stdout);

    if let Some(error) = document
        .as_ref()
        .and_then(|d| d.get("error"))
        .filter(|e| e.is_object())
    {
        return Err(envelope_failure(action, exit, error, stdout, stderr));
    }

    match (action, &document) {
        // 1 is "failed or no runs" and 3 is "running": states, not errors.
        ("ci_status", Some(document)) if matches!(exit, 0 | 1 | 3) => {
            return normalize(action, document, stdout)
        }
        ("ci_publish_preview" | "ci_publish", Some(document))
            if document.get("outcome").is_some() =>
        {
            return publish_result(action, exit, document, expected_commit)
        }
        // The pull stopped on a conflict, so the receipt is the only place
        // that names the paths. (`oak merge --json` leaves them empty.)
        ("pull", Some(document)) if exit == 5 => {
            let message = failure_message(action, None, stdout, stderr, exit);
            return Err(EngineError::new("conflict", message)
                .detail("exit_code", exit.into())
                .detail("oak_code", Value::Null)
                .detail("conflict_paths", array_field(document, "conflict_paths"))
                .detail(
                    "next_commands",
                    array_field(document, "recommended_next_commands"),
                ));
        }
        _ => {}
    }

    if exit != 0 {
        return Err(plain_failure(action, exit, stdout, stderr));
    }

    match action {
        "branch_create" | "branch_switch" | "fetch" | "restore" => Ok(message_result(stdout)),
        "version" => parse_cli_version(stdout)
            .map(|parsed| {
                json!({
                    "version": parsed.version,
                    "channel": parsed.channel,
                    "build": parsed.build,
                })
            })
            .ok_or_else(|| {
                EngineError::new(
                    "cli_failed",
                    format!("unrecognized version: {}", stdout.trim()),
                )
            }),
        // `--continue` and `--abort` print text; they have no receipt.
        "pull" if document.is_none() => Ok(message_result(stdout)),
        _ => match document {
            Some(document) => normalize(action, &document, stdout),
            None => Err(unparseable(action, stdout)),
        },
    }
}

pub fn parse(args: &Args) -> Result<Value, EngineError> {
    let action = args.str("action")?;
    if !ACTIONS.contains(&action) {
        return Err(EngineError::bad_request(format!(
            "unknown action `{action}`"
        )));
    }
    let exit = args.i64("exit_code")?;
    let stdout = args.opt_str("stdout")?.unwrap_or("");
    let stderr = args.opt_str("stderr")?.unwrap_or("");
    parse_inner(
        action,
        exit,
        stdout,
        stderr,
        args.opt_str("expected_commit")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(action: &str, exit: i64, stdout: &str, stderr: &str) -> Result<Value, Value> {
        let args = json!({"action": action, "exit_code": exit, "stdout": stdout, "stderr": stderr});
        parse(&Args::new(&args).unwrap()).map_err(|error| error.to_json())
    }

    fn code(action: &str, exit: i64, stdout: &str, stderr: &str) -> String {
        let error = run(action, exit, stdout, stderr).unwrap_err();
        error["code"].as_str().unwrap().to_string()
    }

    fn envelope(code: &str, message: &str) -> String {
        json!({"schema_version": 1, "error": {"code": code, "message": message,
            "conflict_paths": ["a"], "recommended_next_commands": ["oak x"]}})
        .to_string()
    }

    #[test]
    fn exit_codes_decide() {
        assert_eq!(code("fetch", 3, "", "busy"), "locked");
        assert_eq!(
            code(
                "branch_switch",
                4,
                "",
                "error: Working directory has uncommitted changes"
            ),
            "dirty_tree"
        );
        assert_eq!(code("pull", 5, "", "x"), "conflict");
        assert_eq!(code("merge", 7, "", "x"), "cli_failed");
        assert_eq!(code("fetch", 1, "", "x"), "cli_failed");
        assert_eq!(
            code("status", 2, "", "error: unexpected argument '--json' found"),
            "cli_too_old"
        );
        assert_eq!(code("status", 2, "", "bad usage"), "cli_failed");
        assert_eq!(run("merge", 8, "", "x").unwrap_err()["retryable"], true);
    }

    #[test]
    fn envelope_wins_over_exit_code() {
        assert_eq!(
            code("status", 0, &envelope("repo_locked", "x"), ""),
            "locked"
        );
        assert_eq!(
            code("push", 1, &envelope("auth_required", "x"), ""),
            "not_logged_in"
        );
        assert_eq!(
            code("push", 6, &envelope("auth_denied", "x"), ""),
            "cli_failed"
        );
        assert_eq!(
            code("commit", 4, &envelope("dirty_worktree", "x"), ""),
            "dirty_tree"
        );
        assert_eq!(
            code("merge", 5, &envelope("merge_in_progress", "x"), ""),
            "conflict"
        );
        let error = run(
            "merge",
            5,
            &envelope("conflict_detected", "boom"),
            "ignored",
        )
        .unwrap_err();
        assert_eq!(error["message"], "boom");
        assert_eq!(error["oak_code"], "conflict_detected");
        assert_eq!(error["conflict_paths"], json!(["a"]));
        assert_eq!(error["next_commands"], json!(["oak x"]));
    }

    #[test]
    fn exit_6_reads_the_message() {
        let error = |action, message: &str| run(action, 6, "", message).unwrap_err();
        assert_eq!(
            error(
                "push",
                "error: Server error: Not logged in to https://oak.space. Run `oak login` first."
            )["code"],
            "not_logged_in"
        );
        let offline = error("push", "error: HTTP error: error sending request");
        assert_eq!(
            (&offline["code"], &offline["retryable"]),
            (&json!("network"), &json!(true))
        );
        let refused = error("push", "error: Server error: nope");
        assert_eq!(
            (&refused["code"], &refused["retryable"]),
            (&json!("network"), &json!(false))
        );
        assert_eq!(
            error(
                "merge",
                "Server error: Merge failed: HTTP 412 Precondition Failed\nCI gates this merge"
            )["code"],
            "ci_blocked"
        );
        assert_eq!(
            error(
                "merge",
                "Server error: 1 open important review finding blocks this"
            )["code"],
            "ci_blocked"
        );
        assert_eq!(error("push", "Server error: HTTP 412")["code"], "network");
    }

    #[test]
    fn lenient_json_skips_noise_and_rejects_garbage() {
        assert_eq!(
            lenient_json("warning: x\n{\"a\":1}\ntrailing").unwrap()["a"],
            1
        );
        assert_eq!(lenient_json("[1]").unwrap()[0], 1);
        assert!(lenient_json("").is_none());
        assert!(lenient_json("no json").is_none());
        assert!(lenient_json("{broken").is_none());
        assert_eq!(code("status", 0, "{broken", ""), "cli_failed");
    }

    #[test]
    fn absent_fields_default_and_unknown_fields_drop() {
        let result = run("status", 0, r#"{"branch":"x","new_field":true}"#, "").unwrap();
        assert_eq!(result["branch"], "x");
        assert_eq!(result["head"], Value::Null);
        assert_eq!(result["merge_in_progress"], false);
        assert_eq!(result["changes"], json!([]));
        assert!(result.get("new_field").is_none());
    }

    #[test]
    fn patches_become_numbered_hunks() {
        let patch = "diff --oak a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@ ctx\n one\n-two\n+TWO\n\\ No newline at end of file\n@@ -9 +9,0 @@\n-x\n";
        let hunks = parse_patch(patch);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0]["new_count"], 2);
        assert_eq!(
            hunks[0]["lines"][2],
            json!({"kind": "added", "text": "TWO", "old_line": null, "new_line": 2, "no_newline": true})
        );
        assert_eq!(hunks[1]["old_count"], 1);
        assert_eq!(hunks[1]["new_count"], 0);
        assert!(parse_patch("@@ nonsense @@\n+x\n").is_empty());
        assert!(parse_patch("").is_empty());
        assert_eq!(hunk_header("@@ -a +1 @@"), None);
    }

    #[test]
    fn whoami_states() {
        let doc = |state: &str| {
            json!({"identity": {"state": state, "username": "ada"}, "remote": "r"}).to_string()
        };
        assert_eq!(
            run("whoami", 0, &doc("authenticated"), "").unwrap()["username"],
            "ada"
        );
        assert_eq!(
            code("whoami", 0, &doc("unauthenticated"), ""),
            "not_logged_in"
        );
        assert_eq!(code("whoami", 0, &doc("unreachable"), ""), "network");
        assert_eq!(code("whoami", 0, &doc("unknown"), ""), "cli_failed");
    }

    #[test]
    fn publish_outcomes() {
        let doc = |outcome: &str| {
            json!({"outcome": outcome, "preview": {"commit": "c", "ready": false, "problems": ["runner offline"]}, "authorization": {"run_id": 7, "run_url": "u"}}).to_string()
        };
        let preview = run("ci_publish_preview", 1, &doc("not_ready"), "").unwrap();
        assert_eq!(
            (&preview["outcome"], &preview["ready"]),
            (&json!("not_ready"), &json!(false))
        );
        let published = run("ci_publish", 0, &doc("authorized"), "").unwrap();
        assert_eq!(published["run_id"], 7);
        let refused = run("ci_publish", 6, &doc("cannot_authorize"), "").unwrap_err();
        assert_eq!(refused["code"], "ci_blocked");
        assert_eq!(refused["problems"], json!(["runner offline"]));
        let moved = |expected: &str| {
            let args = json!({"action": "ci_publish", "exit_code": 0, "stdout": doc("authorized"), "expected_commit": expected});
            parse(&Args::new(&args).unwrap()).unwrap()["commit_moved"].clone()
        };
        assert_eq!(moved("c"), false);
        assert_eq!(moved("other"), true);
        assert!(published.get("commit_moved").is_none());
        // A preview's own refusal comes as an error envelope.
        assert_eq!(
            code(
                "ci_publish_preview",
                6,
                &envelope("remote_error", "Server error: 404"),
                ""
            ),
            "network"
        );
    }

    #[test]
    fn plain_text_actions_and_version() {
        assert_eq!(
            run("fetch", 0, "Already up to date\n", "").unwrap()["message"],
            "Already up to date"
        );
        assert_eq!(
            run("pull", 0, "✓ Sync aborted\n", "").unwrap()["message"],
            "✓ Sync aborted"
        );
        let version = run(
            "version",
            0,
            "oak 0.110.0 (nightly 20261106.274889b0)\n",
            "",
        )
        .unwrap();
        assert_eq!(version["channel"], "nightly");
        assert_eq!(code("version", 0, "what", ""), "cli_failed");
        assert_eq!(
            run("fetch", 1, "", "").unwrap_err()["message"],
            "oak fetch exited with code 1"
        );
    }

    #[test]
    fn bad_arguments() {
        let args = json!({"action": "nope", "exit_code": 0});
        assert!(parse(&Args::new(&args).unwrap()).is_err());
        let args = json!({"action": "status"});
        assert!(parse(&Args::new(&args).unwrap()).is_err());
    }
}
