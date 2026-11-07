//! Post-push conflict check: after work reaches the server, ask whether the
//! branch still merges into its parent, so an agent learns about conflicts in
//! the same command that published them instead of being sent back later.
//!
//! The verdict comes from the server's checkout-free `merge-preview`, which is
//! pinned to the head just pushed, always judged against the live target, and
//! filtered by the caller's path policy. The check is advisory: publication
//! has already succeeded, so every failure degrades to `status: "unknown"` and
//! never changes the exit code of the command that pushed.

use std::time::Duration;

use oak_core::{Repository, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands::branch::RemoteIdentity;
use crate::output;

/// Publication already finished, so a slow verdict must not hold the command.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Conflicting paths named in the human warning; JSON carries the full page.
const WARNING_PATH_LIMIT: usize = 10;

pub(crate) const STATUS_CLEAN: &str = "clean";
pub(crate) const STATUS_CONFLICTS: &str = "conflicts";
pub(crate) const STATUS_UNKNOWN: &str = "unknown";

const REASON_OLDER_SERVER: &str = "server_unsupported";

/// One conflicting path, with the server's conflict kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCheckConflict {
    pub path: String,
    pub kind: String,
}

/// `merge_check` on push, commit and finish receipts. Append-only within
/// `schema_version`. `status` is `clean`, `conflicts` or `unknown`; only
/// `conflicts` is a statement that the branch cannot merge as it stands.
#[derive(Debug, Clone, Serialize)]
pub struct MergeCheckJson {
    pub status: &'static str,
    pub target: String,
    pub branch_head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_head: Option<String>,
    pub conflict_file_count: usize,
    pub conflict_files: Vec<MergeCheckConflict>,
    /// More conflicts exist than `conflict_files` lists, because of the
    /// server's page cap or the caller's path policy.
    pub conflict_files_truncated: bool,
    /// Conflicting paths the caller's path policy hides. They still count.
    pub conflicts_restricted: usize,
    /// Why the status is `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recommended_next_commands: Vec<String>,
}

/// Which pushed branch to check.
pub(crate) struct MergeCheckTarget<'a> {
    pub remote_url: &'a str,
    pub owner: &'a str,
    pub repo_name: &'a str,
    pub token: Option<String>,
    pub branch: &'a str,
    pub target: &'a str,
    pub head: &'a str,
}

#[derive(Debug, Deserialize)]
struct PreviewPins {
    target_head: Option<String>,
}

/// The fields of the server's merge-preview this check depends on.
#[derive(Debug, Deserialize)]
struct Verdict {
    pins: PreviewPins,
    reason: Option<String>,
    clean: Option<bool>,
    conflict_file_count: usize,
    #[serde(default)]
    conflict_files: Vec<MergeCheckConflict>,
    #[serde(default)]
    conflict_files_truncated: bool,
    #[serde(default)]
    conflicts_restricted: usize,
}

/// The branch's merge target, when it has one that could conflict.
pub(crate) fn merge_target(repo: &dyn Repository, branch: &str) -> Option<String> {
    let record = repo.get_branch(branch).ok().flatten()?;
    if !matches!(record.status, oak_core::BranchStatus::Open) {
        return None;
    }
    record.parent_branch.filter(|parent| parent != branch)
}

/// Ask the server for the verdict. Never fails: see the module docs.
pub(crate) async fn check(target: MergeCheckTarget<'_>) -> MergeCheckJson {
    let identity = RemoteIdentity {
        remote_url: target.remote_url.to_string(),
        owner: target.owner.to_string(),
        repo_name: target.repo_name.to_string(),
        token: target.token,
    };
    let request = crate::commands::review::server_review::merge_verdict(
        identity,
        target.branch,
        target.target,
        target.head,
    );
    let answer = match tokio::time::timeout(CHECK_TIMEOUT, request).await {
        Ok(answer) => answer,
        Err(_) => return unknown(target.branch, target.target, target.head, "timed_out"),
    };
    from_answer(answer, target.branch, target.target, target.head)
}

fn from_answer(
    answer: Result<Option<Value>>,
    branch: &str,
    target: &str,
    head: &str,
) -> MergeCheckJson {
    let body = match answer {
        Ok(Some(body)) => body,
        Ok(None) => return unknown(branch, target, head, REASON_OLDER_SERVER),
        Err(error) => {
            output::vlog(&format!("merge check failed for {branch}: {error}"));
            return unknown(branch, target, head, "request_failed");
        }
    };
    let verdict: Verdict = match serde_json::from_value(body) {
        Ok(verdict) => verdict,
        Err(error) => {
            output::vlog(&format!("unexpected merge-preview response: {error}"));
            return unknown(branch, target, head, "unexpected_response");
        }
    };
    let status = match verdict.clean {
        Some(true) => STATUS_CLEAN,
        Some(false) => STATUS_CONFLICTS,
        None => STATUS_UNKNOWN,
    };
    let conflicted = status == STATUS_CONFLICTS;
    MergeCheckJson {
        status,
        target: target.to_string(),
        branch_head: head.to_string(),
        target_head: verdict.pins.target_head,
        // An undecided verdict can carry paths that were merely unreadable;
        // reporting them here would read as a confirmed conflict.
        conflict_file_count: if conflicted {
            verdict.conflict_file_count
        } else {
            0
        },
        conflict_files: if conflicted {
            verdict.conflict_files
        } else {
            Vec::new()
        },
        conflict_files_truncated: conflicted && verdict.conflict_files_truncated,
        conflicts_restricted: if conflicted {
            verdict.conflicts_restricted
        } else {
            0
        },
        reason: if status == STATUS_UNKNOWN {
            Some(verdict.reason.unwrap_or_else(|| "undetermined".to_string()))
        } else {
            None
        },
        recommended_next_commands: recommended_commands(status, branch),
    }
}

fn unknown(branch: &str, target: &str, head: &str, reason: &str) -> MergeCheckJson {
    MergeCheckJson {
        status: STATUS_UNKNOWN,
        target: target.to_string(),
        branch_head: head.to_string(),
        target_head: None,
        conflict_file_count: 0,
        conflict_files: Vec::new(),
        conflict_files_truncated: false,
        conflicts_restricted: 0,
        reason: Some(reason.to_string()),
        recommended_next_commands: recommended_commands(STATUS_UNKNOWN, branch),
    }
}

fn recommended_commands(status: &str, branch: &str) -> Vec<String> {
    match status {
        STATUS_CONFLICTS => vec![
            "oak pull --json".to_string(),
            "oak conflict status --json".to_string(),
            "oak pull --continue".to_string(),
            "oak push --json".to_string(),
        ],
        STATUS_UNKNOWN => vec![format!(
            "oak branch review {branch} --remote --merge-preview --json"
        )],
        _ => Vec::new(),
    }
}

impl MergeCheckJson {
    pub(crate) fn has_conflicts(&self) -> bool {
        self.status == STATUS_CONFLICTS
    }

    /// Human narration. Silent when clean; a verdict from a server without
    /// the review API is not worth a line on every push.
    pub(crate) fn report(&self, branch: &str) {
        if self.has_conflicts() {
            output::warning(&self.warning_lines(branch).join("\n"));
        } else if self.status == STATUS_UNKNOWN
            && self.reason.as_deref() != Some(REASON_OLDER_SERVER)
        {
            output::info(&format!(
                "Could not check '{branch}' for conflicts with {} ({}). Run `{}`.",
                self.target,
                self.reason.as_deref().unwrap_or("undetermined"),
                self.recommended_next_commands.join("` then `"),
            ));
        }
    }

    fn warning_lines(&self, branch: &str) -> Vec<String> {
        let count = self.conflict_file_count;
        let mut lines = vec![format!(
            "'{branch}' conflicts with {} in {count} file{} and cannot merge yet",
            self.target,
            if count == 1 { "" } else { "s" },
        )];
        lines.extend(
            self.conflict_files
                .iter()
                .take(WARNING_PATH_LIMIT)
                .map(|file| format!("  {} ({})", file.path, file.kind)),
        );
        let named = self.conflict_files.len().min(WARNING_PATH_LIMIT);
        if count > named {
            lines.push(format!("  ...and {} more", count - named));
        }
        lines.push(
            "Resolve: `oak pull`, fix the conflicts, `oak pull --continue`, then `oak push`"
                .to_string(),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::OakError;
    use serde_json::json;

    fn preview(clean: Value, count: usize, files: Value, reason: Value) -> Result<Option<Value>> {
        Ok(Some(json!({
            "pins": { "branch_head": "b", "target_head": "t" },
            "reason": reason,
            "clean": clean,
            "conflict_file_count": count,
            "conflict_files": files,
            "conflict_files_truncated": false,
            "conflicts_restricted": 0,
            "invariant_violations": [],
        })))
    }

    fn check_of(answer: Result<Option<Value>>) -> MergeCheckJson {
        from_answer(answer, "feature", "main", "b")
    }

    #[test]
    fn clean_verdict_has_no_follow_up() {
        let check = check_of(preview(json!(true), 0, json!([]), Value::Null));
        assert_eq!(check.status, STATUS_CLEAN);
        assert_eq!(check.target_head.as_deref(), Some("t"));
        assert!(check.recommended_next_commands.is_empty());
    }

    #[test]
    fn conflicts_name_the_paths_and_the_way_out() {
        let check = check_of(preview(
            json!(false),
            1,
            json!([{ "path": "src/a.rs", "kind": "text" }]),
            Value::Null,
        ));
        assert!(check.has_conflicts());
        assert_eq!(check.conflict_files[0].path, "src/a.rs");
        assert_eq!(check.recommended_next_commands[0], "oak pull --json");
    }

    #[test]
    fn undecided_verdict_never_reports_unreadable_paths_as_conflicts() {
        let check = check_of(preview(
            Value::Null,
            1,
            json!([{ "path": "src/a.rs", "kind": "blob_unavailable" }]),
            json!("blob_unavailable"),
        ));
        assert_eq!(check.status, STATUS_UNKNOWN);
        assert_eq!(check.reason.as_deref(), Some("blob_unavailable"));
        assert_eq!(check.conflict_file_count, 0);
        assert!(check.conflict_files.is_empty());
    }

    #[test]
    fn failures_degrade_to_unknown() {
        assert_eq!(
            check_of(Ok(None)).reason.as_deref(),
            Some(REASON_OLDER_SERVER)
        );
        assert_eq!(
            check_of(Err(OakError::Server("boom".to_string())))
                .reason
                .as_deref(),
            Some("request_failed")
        );
        assert_eq!(
            check_of(Ok(Some(json!({ "surprise": true }))))
                .reason
                .as_deref(),
            Some("unexpected_response")
        );
    }

    #[test]
    fn only_open_branches_with_a_parent_have_a_merge_target() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let open = oak_core::Branch::new("open".into(), None, Some("main".into()));
        repo.store_branch(&open).unwrap();
        let rootless = oak_core::Branch::new("rootless".into(), None, None);
        repo.store_branch(&rootless).unwrap();
        assert_eq!(merge_target(&repo, "open").as_deref(), Some("main"));
        assert_eq!(merge_target(&repo, "rootless"), None);
        assert_eq!(merge_target(&repo, "missing"), None);
    }

    #[test]
    fn warning_caps_named_paths_and_counts_the_rest() {
        let files: Vec<Value> = (0..12)
            .map(|i| json!({ "path": format!("f{i}"), "kind": "text" }))
            .collect();
        let check = check_of(preview(json!(false), 15, json!(files), Value::Null));
        let lines = check.warning_lines("feature");
        assert!(lines[0].contains("in 15 files"));
        assert_eq!(lines.iter().filter(|l| l.ends_with("(text)")).count(), 10);
        assert!(lines.iter().any(|l| l.contains("and 5 more")));
    }
}
