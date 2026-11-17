//! `oak close --remote A@HEAD B@HEAD ... --json` (fb-413): checkout-free,
//! exact-head fenced closing of several remote branches in one invocation.
//!
//! Each branch is closed independently through the same metadata-only
//! publication the single-branch `oak close --remote` uses, so every close is
//! atomic per branch and a failure on one branch never rolls back or blocks
//! another. There is deliberately no all-or-nothing batch semantics.
//!
//! The fence is a read-then-close: the server's branch-close paths (the
//! metadata-only push and `POST .../branches/:b/close`) carry no expected-head
//! precondition today, so the head is observed immediately before each close
//! and compared with the caller's exact pin. A push that lands between that
//! read and the close is not prevented; it is detected afterwards by the final
//! branch listing (`head_moved_after_fence`). The receipt states this race
//! window explicitly instead of claiming a server-side CAS.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use oak_core::{BranchStatus, CloseReason, OakError, Result};
use serde::Serialize;

use super::branch::{RemoteBranchData, RemoteWorkspace};
use crate::output;

/// Upper bound on targets per invocation. Each target costs one branch listing
/// plus one publication request, so keep the request count finite.
pub const MAX_CLOSE_TARGETS: usize = 100;

/// One `BRANCH@FULL_HEAD` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseTarget {
    pub branch: String,
    pub expected_head: String,
}

fn is_full_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// True when an argument looks like an exact-head `BRANCH@HEAD` target,
/// including malformed pins (short or uppercase hex), so those reach the
/// typed refusal in [`parse_targets`] instead of a branch-name lookup.
pub fn is_fenced_target(arg: &str) -> bool {
    arg.rsplit_once('@').is_some_and(|(branch, head)| {
        !branch.is_empty() && head.len() >= 4 && head.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Whether `arg` should be treated as a pinned `BRANCH@HEAD` target. A full
/// 64-hex suffix (any case) always is. A shorter hex suffix is ambiguous with
/// a branch literally named `name@abc123` (valid branch names may contain
/// `@`), so it is treated as a pin — and refused with the typed short-hash
/// error — only when no local branch has that literal name.
pub fn looks_pinned(arg: &str, literal_branch_exists: impl FnOnce(&str) -> bool) -> bool {
    let full = arg.rsplit_once('@').is_some_and(|(branch, head)| {
        !branch.is_empty() && head.len() == 64 && head.bytes().all(|b| b.is_ascii_hexdigit())
    });
    full || (is_fenced_target(arg) && !literal_branch_exists(arg))
}

/// Whether this checkout has a local branch literally named `name`.
pub fn local_branch_exists(path: &Path, name: &str) -> bool {
    crate::resolve::resolve(path)
        .and_then(|ctx| ctx.open())
        .and_then(|repo| repo.get_branch(name))
        .is_ok_and(|branch| branch.is_some())
}

/// Parse `BRANCH@FULL_HEAD` arguments. Every argument must carry a full
/// lowercase 64-hex head: an unfenced close among fenced ones would silently
/// defeat the point of the fence.
pub fn parse_targets(args: &[String]) -> Result<Vec<CloseTarget>> {
    if args.is_empty() {
        return Err(OakError::InvalidArgument(
            "no branches given; pass BRANCH@FULL_HEAD for each branch to close".to_string(),
        ));
    }
    if args.len() > MAX_CLOSE_TARGETS {
        return Err(OakError::InvalidArgument(format!(
            "at most {MAX_CLOSE_TARGETS} branches can be closed per invocation"
        )));
    }
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(args.len());
    for arg in args {
        let Some((branch, head)) = arg.rsplit_once('@') else {
            return Err(OakError::InvalidArgument(format!(
                "`{arg}` has no exact head; closing several branches requires BRANCH@FULL_HEAD for every branch (read heads with `oak branch list --remote --status open --json`)"
            )));
        };
        if !is_full_hash(head) {
            return Err(OakError::InvalidArgument(format!(
                "`{arg}`: the head after `@` must be a full lowercase 64-character Oak commit hash"
            )));
        }
        super::branch::validate_branch_name(branch)?;
        if !seen.insert(branch.to_string()) {
            return Err(OakError::InvalidArgument(format!(
                "branch `{branch}` is listed more than once"
            )));
        }
        targets.push(CloseTarget {
            branch: branch.to_string(),
            expected_head: head.to_string(),
        });
    }
    Ok(targets)
}

/// Typed per-branch outcome. Only `closed` and `already_closed` mean the
/// branch is closed because of (or regardless of) this invocation;
/// `unknown_outcome` means a close request was sent and its result could not
/// be confirmed, so it must be reconciled read-only, never blindly retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseOutcome {
    Closed,
    /// The close landed, but the final listing shows a head other than the
    /// pin: a push raced the read-then-close fence, so the branch was closed
    /// with commits nobody reviewed. Inspect it and reopen if needed.
    ClosedAfterHeadMoved,
    AlreadyClosed,
    RefusedHeadMoved,
    NotFound,
    Rejected,
    ReadFailed,
    UnknownOutcome,
}

impl CloseOutcome {
    fn mutation_sent(self) -> bool {
        matches!(
            self,
            CloseOutcome::Closed
                | CloseOutcome::ClosedAfterHeadMoved
                | CloseOutcome::Rejected
                | CloseOutcome::UnknownOutcome
        )
    }
}

#[derive(Debug, Serialize)]
struct CloseRow {
    branch: String,
    expected_head: String,
    outcome: CloseOutcome,
    /// Head observed immediately before the close decision (null when the
    /// branch has no head or could not be read).
    observed_head: Option<String>,
    /// Remote status observed immediately before the close decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_status: Option<String>,
    /// Whether a close request was sent to the server for this branch.
    close_request_sent: bool,
    /// Status in the final listing (reconciliation evidence, not a verdict:
    /// an `unknown_outcome` stays unknown even when this reads `closed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    final_observed_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_observed_head: Option<String>,
    /// True when the final listing shows a different head than the pin: a
    /// push raced the read-then-close fence. Only set for rows this run closed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    head_moved_after_fence: bool,
    /// Rows this run closed only: `true` when the final listing confirmed the
    /// pinned head, `false` when it showed a moved head, `"unchecked"` when
    /// the final listing was unavailable or omitted the branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    fence_verified: Option<serde_json::Value>,
    /// Set when restoring the local mirror after an unconfirmed or rejected
    /// close failed; the local row may then disagree with the remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_rollback_error: Option<String>,
    /// How this checkout's local head for the branch relates to the remote
    /// head. A remote close never moves a local head that is ahead of or
    /// diverged from the remote; those local-only commits are kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_head: Option<super::branch::LocalHeadRelation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_ahead: Option<usize>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    local_only_commits: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OpenBranchRow {
    name: String,
    head: Option<String>,
}

#[derive(Debug, Serialize)]
struct FenceJson {
    kind: &'static str,
    server_precondition: bool,
    race_window: &'static str,
}

#[derive(Debug, Serialize)]
struct CloseManyJson {
    schema_version: u32,
    operation: &'static str,
    remote: bool,
    repository: String,
    close_reason: Option<String>,
    fence: FenceJson,
    all_closed: bool,
    counts: BTreeMap<&'static str, usize>,
    results: Vec<CloseRow>,
    /// Open branches in the final listing, read after every close attempt.
    /// `null` when that listing could not be read.
    open_branches: Option<Vec<OpenBranchRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_branches_unavailable_reason: Option<String>,
    recommended_next_commands: Vec<String>,
}

fn outcome_name(outcome: CloseOutcome) -> &'static str {
    match outcome {
        CloseOutcome::Closed => "closed",
        CloseOutcome::ClosedAfterHeadMoved => "closed_after_head_moved",
        CloseOutcome::AlreadyClosed => "already_closed",
        CloseOutcome::RefusedHeadMoved => "refused_head_moved",
        CloseOutcome::NotFound => "not_found",
        CloseOutcome::Rejected => "rejected",
        CloseOutcome::ReadFailed => "read_failed",
        CloseOutcome::UnknownOutcome => "unknown_outcome",
    }
}

/// Exit code for the whole invocation: 0 only when every branch ended closed
/// at its pinned head as confirmed by the final listing (or was already
/// closed); 6 when any result is transport-class/unconfirmed, including a
/// close whose fence could not be re-checked (reconcile before acting);
/// otherwise 5 (a refusal, rejection, or close that lost the fence race).
fn exit_code_for(results: &[CloseRow]) -> i32 {
    let unchecked = |row: &CloseRow| {
        row.outcome == CloseOutcome::Closed && row.fence_verified.as_ref() != Some(&true.into())
    };
    if results.iter().all(|row| {
        row.outcome == CloseOutcome::AlreadyClosed
            || (row.outcome == CloseOutcome::Closed && !unchecked(row))
    }) {
        0
    } else if results.iter().any(|row| {
        matches!(
            row.outcome,
            CloseOutcome::UnknownOutcome | CloseOutcome::ReadFailed
        ) || unchecked(row)
    }) {
        6
    } else {
        5
    }
}

/// Wall bound for each branch listing (the shared API client has none).
const LISTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn list_branches(
    remote: &super::branch::RemoteIdentity,
) -> std::result::Result<Vec<RemoteBranchData>, String> {
    match tokio::time::timeout(
        LISTING_TIMEOUT,
        super::branch::fetch_remote_branches(remote),
    )
    .await
    {
        Ok(Ok(branches)) => Ok(branches),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!(
            "branch listing timed out after {}s",
            LISTING_TIMEOUT.as_secs()
        )),
    }
}

fn find<'a>(branches: &'a [RemoteBranchData], name: &str) -> Option<&'a RemoteBranchData> {
    branches.iter().find(|branch| branch.name == name)
}

/// Close each target on the configured remote, fenced by its exact head.
/// Prints one JSON document and returns the process exit code.
pub async fn close_many_remote_json(
    path: &Path,
    targets: &[CloseTarget],
    close_reason: Option<CloseReason>,
) -> Result<i32> {
    let workspace = RemoteWorkspace::open(path)?;
    let repo = workspace.repo;
    let remote = workspace.remote;
    let repository = format!("{}/{}", remote.owner, remote.repo_name);
    let reconcile = "oak branch list --remote --json".to_string();

    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let mut row = CloseRow {
            branch: target.branch.clone(),
            expected_head: target.expected_head.clone(),
            outcome: CloseOutcome::ReadFailed,
            observed_head: None,
            observed_status: None,
            close_request_sent: false,
            final_observed_status: None,
            final_observed_head: None,
            head_moved_after_fence: false,
            fence_verified: None,
            local_rollback_error: None,
            local_head: None,
            local_ahead: None,
            local_only_commits: false,
            detail: None,
            recommended_next_commands: Vec::new(),
        };
        // Read as late as possible: a fresh listing per target keeps the
        // read-then-close window to one round trip for every branch.
        let mut branches = match list_branches(&remote).await {
            Ok(branches) => branches,
            Err(error) => {
                row.detail = Some(format!(
                    "branch listing failed before any close request: {error}"
                ));
                row.recommended_next_commands.push(reconcile.clone());
                results.push(row);
                continue;
            }
        };
        let Some(observed) = find(&branches, &target.branch) else {
            row.outcome = CloseOutcome::NotFound;
            row.detail = Some("branch is not present on the remote".to_string());
            results.push(row);
            continue;
        };
        row.observed_head = observed.head.clone();
        row.observed_status = Some(observed.status.clone());
        if BranchStatus::from_db_str(&observed.status) == BranchStatus::Closed {
            row.outcome = CloseOutcome::AlreadyClosed;
            if observed.head.as_deref() != Some(target.expected_head.as_str()) {
                row.detail = Some(
                    "branch was already closed; its head differs from the pin (not modified)"
                        .to_string(),
                );
            }
            results.push(row);
            continue;
        }
        if observed.head.as_deref() != Some(target.expected_head.as_str()) {
            row.outcome = CloseOutcome::RefusedHeadMoved;
            row.detail = Some(
                "remote head differs from the pinned head; nothing was sent for this branch"
                    .to_string(),
            );
            row.recommended_next_commands.push(format!(
                "oak branch show {} --remote --json",
                output::shell_quote(&target.branch)
            ));
            results.push(row);
            continue;
        }

        let observed_generation = match repo.description_generation() {
            Ok(generation) => generation,
            Err(error) => {
                row.outcome = CloseOutcome::ReadFailed;
                row.detail = Some(format!("local metadata could not be read: {error}"));
                results.push(row);
                continue;
            }
        };
        match super::branch::store_remote_branch_metadata(
            &repo,
            &mut branches,
            &target.branch,
            Some(BranchStatus::Closed),
            close_reason.clone(),
            observed_generation,
        ) {
            Ok(relation) => {
                if let super::branch::LocalHeadRelation::LocalAhead { local_ahead } = relation {
                    row.local_ahead = Some(local_ahead);
                }
                if relation.has_local_only_commits() {
                    row.local_only_commits = true;
                    row.recommended_next_commands.push(format!(
                        "oak branch show {} --json",
                        output::shell_quote(&target.branch)
                    ));
                }
                row.local_head = Some(relation);
            }
            Err(error) => {
                row.outcome = CloseOutcome::ReadFailed;
                row.detail = Some(format!(
                    "local branch row could not be prepared; no close request was sent: {error}"
                ));
                results.push(row);
                continue;
            }
        }

        row.close_request_sent = true;
        match super::push::push_branch_metadata(
            &repo,
            &remote.remote_url,
            &remote.owner,
            &remote.repo_name,
            &target.branch,
            remote.token.as_deref(),
        )
        .await
        {
            Ok(()) => row.outcome = CloseOutcome::Closed,
            Err(error @ (OakError::Server(_) | OakError::AuthRequired { .. })) => {
                row.outcome = CloseOutcome::Rejected;
                row.detail = Some(error.to_string());
            }
            Err(error) => {
                // Any other failure after the request may have been sent is
                // ambiguous. Never retry: reconcile read-only first.
                row.outcome = CloseOutcome::UnknownOutcome;
                row.detail = Some(error.to_string());
                row.recommended_next_commands.push(reconcile.clone());
            }
        }
        if row.outcome != CloseOutcome::Closed {
            // The local mirror was marked closed (with the reason) to build
            // the request. Only a confirmed close may stay recorded; restore
            // the row to exactly what the remote showed before the attempt.
            let restored = repo.description_generation().and_then(|generation| {
                super::branch::store_remote_branch_metadata(
                    &repo,
                    &mut branches,
                    &target.branch,
                    None,
                    None,
                    generation,
                )
            });
            if let Err(error) = restored {
                row.local_rollback_error = Some(format!(
                    "the local branch row still reads closed and may disagree with the remote: {error}"
                ));
            }
        }
        results.push(row);
    }

    // Final listing: the open-branch list the caller asked for, plus
    // post-hoc detection of pushes that raced the read-then-close fence.
    let (open_branches, open_branches_unavailable_reason) = match list_branches(&remote).await {
        Ok(final_branches) => {
            for row in &mut results {
                if let Some(branch) = find(&final_branches, &row.branch) {
                    row.final_observed_status = Some(branch.status.clone());
                    row.final_observed_head = branch.head.clone();
                    if row.outcome == CloseOutcome::Closed {
                        let moved = branch.head.as_deref() != Some(row.expected_head.as_str());
                        row.fence_verified = Some((!moved).into());
                        if moved {
                            row.head_moved_after_fence = true;
                            row.outcome = CloseOutcome::ClosedAfterHeadMoved;
                            row.detail = Some(
                                    "closed, but the remote head moved past the pin during the close (read-then-close race): the branch was closed with unreviewed commits; inspect it and reopen it if that work is still wanted"
                                        .to_string(),
                                );
                            row.recommended_next_commands.push(format!(
                                "oak branch show {} --remote --json",
                                output::shell_quote(&row.branch)
                            ));
                        }
                    }
                }
            }
            let mut open: Vec<OpenBranchRow> = final_branches
                .into_iter()
                .filter(|branch| BranchStatus::from_db_str(&branch.status) == BranchStatus::Open)
                .map(|branch| OpenBranchRow {
                    name: branch.name,
                    head: branch.head,
                })
                .collect();
            open.sort_by(|a, b| a.name.cmp(&b.name));
            (Some(open), None)
        }
        Err(error) => (None, Some(error)),
    };
    for row in &mut results {
        if row.outcome == CloseOutcome::Closed && row.fence_verified.is_none() {
            row.fence_verified = Some("unchecked".into());
            row.detail = Some(
                "closed, but the final listing could not confirm the head was still the pin; reconcile read-only"
                    .to_string(),
            );
            row.recommended_next_commands.push(reconcile.clone());
        }
    }

    let mut counts = BTreeMap::new();
    for row in &results {
        *counts.entry(outcome_name(row.outcome)).or_insert(0) += 1;
    }
    let code = exit_code_for(&results);
    let mut recommended_next_commands =
        vec!["oak branch list --remote --status open --json".to_string()];
    if results.iter().any(|row| {
        row.outcome == CloseOutcome::UnknownOutcome
            || row
                .fence_verified
                .as_ref()
                .is_some_and(|v| v == "unchecked")
    }) {
        recommended_next_commands.insert(0, reconcile.clone());
    }
    let payload = CloseManyJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        operation: "close_many",
        remote: true,
        repository,
        close_reason: close_reason.as_ref().map(|r| r.as_str().to_string()),
        fence: FenceJson {
            kind: "read_then_close",
            server_precondition: false,
            race_window: "the server close carries no expected-head precondition; a push landing between the per-branch head read and its close is not prevented, only detected afterwards by the final listing (outcome closed_after_head_moved, fence_verified false; \"unchecked\" when that listing is unavailable)",
        },
        all_closed: code == 0,
        counts,
        results,
        open_branches,
        open_branches_unavailable_reason,
        recommended_next_commands,
    };
    debug_assert!(payload
        .results
        .iter()
        .all(|row| row.close_request_sent == row.outcome.mutation_sent()));
    output::print_json(&payload)?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_exact_head_targets_and_branch_names_containing_at() {
        let targets = parse_targets(&[format!("a@{H}"), format!("x@y@{H}")]).unwrap();
        assert_eq!(targets[0].branch, "a");
        assert_eq!(targets[1].branch, "x@y");
        assert_eq!(targets[1].expected_head, H);
        assert!(is_fenced_target(&format!("a@{H}")));
        assert!(!is_fenced_target("a@HEAD"));
        // Malformed pins still route to the typed refusal.
        assert!(is_fenced_target("a@1111111"));
        assert!(is_fenced_target(&format!("a@{}", H.to_uppercase())));
        // Short hex is a pin only when no branch has the literal name.
        assert!(looks_pinned("a@1111111", |_| false));
        assert!(!looks_pinned("a@1111111", |_| true));
        // A full hash is always a pin.
        assert!(looks_pinned(&format!("a@{H}"), |_| true));
        assert!(!looks_pinned("release@v2", |_| false));
    }

    #[test]
    fn refuses_unfenced_short_duplicate_or_main_targets() {
        for args in [
            vec!["a".to_string()],
            vec!["a@abc".to_string()],
            vec![format!("a@{}", H.to_uppercase())],
            vec![format!("a@{H}"), format!("a@{H}")],
            vec![format!("main@{H}")],
            vec![],
        ] {
            assert!(
                matches!(parse_targets(&args), Err(OakError::InvalidArgument(_))),
                "{args:?} must be refused"
            );
        }
    }

    #[test]
    fn exit_code_separates_success_refusal_and_ambiguity() {
        let row = |outcome| CloseRow {
            branch: "b".into(),
            expected_head: H.into(),
            outcome,
            observed_head: None,
            observed_status: None,
            close_request_sent: false,
            final_observed_status: None,
            final_observed_head: None,
            head_moved_after_fence: false,
            fence_verified: Some(true.into()),
            local_rollback_error: None,
            local_head: None,
            local_ahead: None,
            local_only_commits: false,
            detail: None,
            recommended_next_commands: vec![],
        };
        assert_eq!(
            exit_code_for(&[row(CloseOutcome::Closed), row(CloseOutcome::AlreadyClosed)]),
            0
        );
        assert_eq!(
            exit_code_for(&[
                row(CloseOutcome::Closed),
                row(CloseOutcome::RefusedHeadMoved)
            ]),
            5
        );
        assert_eq!(
            exit_code_for(&[
                row(CloseOutcome::RefusedHeadMoved),
                row(CloseOutcome::UnknownOutcome)
            ]),
            6
        );
        // A lone close that lost the fence race is never success.
        assert_eq!(exit_code_for(&[row(CloseOutcome::ClosedAfterHeadMoved)]), 5);
        // A close whose fence could not be re-checked is unconfirmed.
        let mut unchecked = row(CloseOutcome::Closed);
        unchecked.fence_verified = Some("unchecked".into());
        assert_eq!(exit_code_for(&[unchecked]), 6);
    }
}
