use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use oak_core::{
    BranchStatus, ChangeType, FileChange, Manifest, MetadataKey, OakError, Repository, Result,
    SqliteRepository, DEFAULT_BRANCH,
};
use serde::Serialize;

use crate::agent_action::{
    AgentActionContext, AgentActionDetailInput, AgentActionState, RemoteFreshnessInput,
};
use crate::commands::branch::{
    fetch_remote_branches, RemoteBranchData, RemoteIdentity, RemoteWorkspace,
};
use crate::commands::review::{branch_triage_evidence, BranchComparison, MergePreviewJson};
use crate::output;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Close,
    ValidateThenMerge,
    Resolve,
    Rebuild,
    /// Hard-fail: the four-tree merge-safety classification found a merge
    /// invariant violation — the predicted result destroys target-side
    /// state the branch never touched (fb-105).
    DoNotMerge,
    Review,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Clean,
    Conflicts,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionState {
    Empty,
    SupersededExact,
    Contributes,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetRisk {
    None,
    RevertsTargetExact,
    Unknown,
}

/// Required-check evidence for one exact branch head (fb-120/fb-327).
///
/// This is an observation of the CI runs listing, bound to the exact head
/// commit; it is never authorization. `known_passed` is true only for a
/// concluded success of the newest run per workflow at that exact commit.
#[derive(Debug, Serialize)]
pub struct ChecksJson {
    pub required: bool,
    pub known_passed: bool,
    /// `ci_run:<id>@<head>` when a run at the exact head decided the state,
    /// `ci_runs_listing` when the listing was read but held no run for the
    /// head, `unavailable` when CI could not be read, null when not queried.
    pub source: Option<String>,
    /// `success`, `failure`, `running`, `no_runs`, `unavailable`, or
    /// `not_queried`.
    pub state: &'static str,
    /// The exact commit the evidence is bound to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ChecksJson {
    /// No CI evidence was gathered (the pre-fb-120 shape, with a reason).
    pub fn not_queried(reason: &str) -> Self {
        Self {
            required: true,
            known_passed: false,
            source: None,
            state: CHECKS_NOT_QUERIED,
            head: None,
            run_id: None,
            run_ids: Vec::new(),
            run_url: None,
            binding: None,
            scan_limit: None,
            reason: Some(reason.to_string()),
        }
    }
}

/// Per-branch error for a row that could not be assessed (fb-118). Carries
/// a stable code and a bounded message; never a raw storage/parser dump.
#[derive(Debug, Clone, Serialize)]
pub struct TriageRowErrorJson {
    pub code: &'static str,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<&'static str>,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct UniqueContributionJson {
    pub changed_file_count: usize,
    pub changed_paths_sample: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BranchTriageJson {
    pub recommended_action: RecommendedAction,
    pub recommended_action_detail: output::AgentRecommendedActionJson,
    pub reason: String,
    pub confidence: &'static str,
    pub analysis_depth: &'static str,
    pub analysis_budget_exhausted: bool,
    pub next_detail_command: String,
    pub close_allowed: bool,
    pub vcs_merge_safe: Option<bool>,
    pub merge_allowed: bool,
    pub checks: ChecksJson,
    pub mergeability: Mergeability,
    pub contribution: ContributionState,
    pub target_risk: TargetRisk,
    pub unique_contribution: UniqueContributionJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_data: Vec<String>,
}

pub struct BranchTriageInput<'a> {
    pub branch: &'a str,
    pub remote: bool,
    pub branch_commit_count: usize,
    pub changed_files: &'a [FileChange],
    pub branch_manifest: &'a Manifest,
    pub against_manifest: &'a Manifest,
    pub missing_blob_paths: &'a [String],
    pub local_snapshot_missing: bool,
    pub against_head_missing: bool,
    pub(crate) merge_preview: &'a MergePreviewJson,
}

pub fn derive_branch_triage(
    repo: &dyn Repository,
    input: BranchTriageInput<'_>,
) -> Result<BranchTriageJson> {
    let remote_flag = if input.remote { " --remote" } else { "" };
    let next_detail_command = format!(
        "oak branch review {}{} --merge-preview --json",
        input.branch, remote_flag
    );

    let mut missing_data = Vec::new();
    if input.local_snapshot_missing {
        missing_data.push("local_snapshot_unavailable".to_string());
    }
    if input.against_head_missing {
        missing_data.push("against_head_unavailable".to_string());
    }
    for path in input.missing_blob_paths {
        missing_data.push(format!("missing_blob:{path}"));
    }
    if !input.merge_preview.prediction_available {
        missing_data.push("merge_prediction_unavailable".to_string());
    }

    let mergeability = mergeability_from_preview(input.merge_preview);
    let contribution = contribution_state(
        repo,
        input.branch_commit_count,
        input.changed_files,
        input.missing_blob_paths,
        input.local_snapshot_missing,
        input.branch_manifest,
        input.against_manifest,
        input.against_head_missing,
    )?;
    let target_risk = target_risk_from_preview(input.merge_preview);

    let unique_paths: Vec<String> = input
        .changed_files
        .iter()
        .map(|change| change.path.clone())
        .collect();
    let unique_contribution = UniqueContributionJson {
        changed_file_count: unique_paths.len(),
        changed_paths_sample: unique_paths.iter().take(20).cloned().collect(),
    };

    let has_merge_safety_unknown = mergeability == Mergeability::Unknown
        || target_risk == TargetRisk::Unknown
        || contribution == ContributionState::Unknown
        || input.local_snapshot_missing
        || !input.missing_blob_paths.is_empty()
        || input.against_head_missing;

    let close_allowed = input.missing_blob_paths.is_empty()
        && !input.against_head_missing
        && matches!(
            contribution,
            ContributionState::Empty | ContributionState::SupersededExact
        );

    let vcs_merge_safe = if preview_invariant_violations(input.merge_preview).is_some() {
        // An invariant violation is merge-unsafe whatever the contribution
        // state says (fb-105).
        Some(false)
    } else if matches!(
        contribution,
        ContributionState::Empty | ContributionState::SupersededExact
    ) {
        // Close-recommended branches make no merge-safety claim at all.
        None
    } else {
        // vcs_merge_safe_from_preview fails closed on uncertified
        // predictions: it can return Some(false) or None, never Some(true),
        // without an authoritative target head (fb-105).
        vcs_merge_safe_from_preview(input.merge_preview)
    };

    let (recommended_action, reason, confidence, analysis_depth) = recommend_action(
        input.branch,
        contribution,
        mergeability,
        target_risk,
        close_allowed,
        has_merge_safety_unknown,
        vcs_merge_safe,
        input.merge_preview,
    );
    let remote_configured = input.remote || repo.get_metadata(MetadataKey::RemoteUrl)?.is_some();
    let recommended_action_detail = recommended_action_detail(RecommendedActionDetailInput {
        branch: input.branch,
        remote: input.remote,
        remote_configured,
        remote_degraded: false,
        action: recommended_action,
        confidence,
        reason: &reason,
        next_detail_command: &next_detail_command,
    });

    Ok(BranchTriageJson {
        recommended_action,
        recommended_action_detail,
        reason,
        confidence,
        analysis_depth,
        analysis_budget_exhausted: false,
        next_detail_command,
        close_allowed,
        vcs_merge_safe,
        // Filled from exact-head CI evidence by `apply_checks` (fb-120).
        merge_allowed: false,
        checks: ChecksJson::not_queried(CHECKS_REASON_NOT_REQUESTED),
        mergeability,
        contribution,
        target_risk,
        unique_contribution,
        missing_data,
    })
}

/// A local review of the current branch must not recommend closing committed
/// history while that same checkout contains unfinished work. Remote and
/// non-current branch reviews have no authority to make a cleanliness claim,
/// so callers invoke this only for an exact current-branch match.
pub(crate) fn guard_dirty_current_worktree(
    repo: &dyn Repository,
    branch: &str,
    triage: &mut BranchTriageJson,
) -> Result<()> {
    triage.close_allowed = false;
    if triage.recommended_action != RecommendedAction::Close {
        return Ok(());
    }

    triage.recommended_action = RecommendedAction::Review;
    triage.reason = "worktree_dirty".to_string();
    triage.confidence = "high";
    triage.analysis_depth = "worktree";
    triage.recommended_action_detail = recommended_action_detail(RecommendedActionDetailInput {
        branch,
        remote: false,
        remote_configured: repo.get_metadata(MetadataKey::RemoteUrl)?.is_some(),
        remote_degraded: false,
        action: RecommendedAction::Review,
        confidence: triage.confidence,
        reason: &triage.reason,
        next_detail_command: "oak status --json",
    });
    Ok(())
}

struct RecommendedActionDetailInput<'a> {
    branch: &'a str,
    remote: bool,
    remote_configured: bool,
    remote_degraded: bool,
    action: RecommendedAction,
    confidence: &'static str,
    reason: &'a str,
    next_detail_command: &'a str,
}

fn recommended_action_detail(
    input: RecommendedActionDetailInput<'_>,
) -> output::AgentRecommendedActionJson {
    use output::AgentRecommendedActionKindJson as Kind;

    let remote_flag = if input.remote { " --remote" } else { "" };
    let refresh_errors = if input.remote_degraded {
        vec!["branch_triage_degraded".to_string()]
    } else {
        Vec::new()
    };
    let (kind, command, blocking_reason, risk_notes) = match input.action {
        RecommendedAction::Close => (
            Kind::CloseBranch,
            format!(
                "oak close {}{remote_flag} --reason stale --json",
                input.branch
            ),
            None,
            vec![input.reason.to_string(), "updates_branch_state".to_string()],
        ),
        RecommendedAction::ValidateThenMerge => (
            Kind::Merge,
            format!("oak merge {}", input.branch),
            None,
            vec![
                "updates_parent_branch".to_string(),
                "requires_checks_before_merge".to_string(),
            ],
        ),
        RecommendedAction::Resolve => (
            Kind::ResolveConflict,
            format!("oak switch {}", input.branch),
            Some("merge_conflicts".to_string()),
            vec![
                "merge_conflicts_predicted".to_string(),
                "switch_refuses_dirty_worktree".to_string(),
            ],
        ),
        RecommendedAction::Rebuild => (
            Kind::ReviewBranch,
            input.next_detail_command.to_string(),
            Some(input.reason.to_string()),
            vec!["branch_reverts_target".to_string()],
        ),
        RecommendedAction::DoNotMerge => (
            Kind::ReviewBranch,
            input.next_detail_command.to_string(),
            Some(input.reason.to_string()),
            vec![
                "merge_invariant_violation".to_string(),
                "merge_would_destroy_target_side_state".to_string(),
            ],
        ),
        RecommendedAction::Review => (
            Kind::ReviewBranch,
            input.next_detail_command.to_string(),
            Some(input.reason.to_string()),
            vec!["needs_more_evidence".to_string()],
        ),
        RecommendedAction::Unknown => (
            Kind::ReviewBranch,
            input.next_detail_command.to_string(),
            Some(input.reason.to_string()),
            vec!["unclassified_branch".to_string()],
        ),
    };

    crate::agent_action::build(AgentActionDetailInput {
        context: AgentActionContext::BranchTriage {
            remote: input.remote,
        },
        command,
        kind: Some(kind),
        state: AgentActionState::default(),
        remote: RemoteFreshnessInput {
            remote_configured: input.remote || input.remote_configured,
            refresh_requested: input.remote,
            refresh_supported: true,
            refresh_errors: &refresh_errors,
            remote_parent_fetched_at: None,
            current_branch_push_checked: false,
        },
        blocking_reason,
        risk_hints: risk_notes,
        confidence_hint: Some(input.confidence),
    })
}

pub fn prove_superseded_paths(
    repo: &dyn Repository,
    changes: &[FileChange],
    against_manifest: &Manifest,
    branch_manifest: &Manifest,
) -> Result<(bool, Vec<String>)> {
    if changes.is_empty() {
        return Ok((true, Vec::new()));
    }

    let mut unproven = Vec::new();
    for change in changes {
        if !path_superseded(repo, change, against_manifest, branch_manifest)? {
            unproven.push(change.path.clone());
        }
    }
    Ok((unproven.is_empty(), unproven))
}

fn path_superseded(
    repo: &dyn Repository,
    change: &FileChange,
    against_manifest: &Manifest,
    branch_manifest: &Manifest,
) -> Result<bool> {
    match change.change_type {
        ChangeType::Deleted => Ok(against_manifest.get(&change.path).is_none()),
        ChangeType::Added | ChangeType::Modified | ChangeType::Renamed => {
            let Some(branch_entry) = branch_manifest.get(&change.path) else {
                return Ok(false);
            };
            match against_manifest.get(&change.path) {
                Some(against_entry) => {
                    if against_entry.blob_hash == branch_entry.blob_hash
                        && against_entry.mode == branch_entry.mode
                    {
                        return Ok(true);
                    }
                    if text_hunk_included(repo, &branch_entry.blob_hash, &against_entry.blob_hash)?
                    {
                        return Ok(true);
                    }
                    Ok(false)
                }
                None => Ok(false),
            }
        }
    }
}

fn text_hunk_included(
    repo: &dyn Repository,
    branch_blob: &oak_core::Hash,
    against_blob: &oak_core::Hash,
) -> Result<bool> {
    let Some(branch_bytes) = repo.get_blob(branch_blob)?.map(|b| b.content) else {
        return Ok(false);
    };
    let Some(against_bytes) = repo.get_blob(against_blob)?.map(|b| b.content) else {
        return Ok(false);
    };
    let (Ok(branch_text), Ok(against_text)) = (
        std::str::from_utf8(&branch_bytes),
        std::str::from_utf8(&against_bytes),
    ) else {
        return Ok(branch_bytes == against_bytes);
    };
    Ok(simple_text_hunks_included(branch_text, against_text))
}

fn simple_text_hunks_included(branch_text: &str, against_text: &str) -> bool {
    if branch_text == against_text {
        return true;
    }
    let branch_lines: Vec<&str> = branch_text.lines().collect();
    if branch_lines.is_empty() {
        return against_text.is_empty();
    }
    let mut search_from = 0usize;
    for line in branch_lines {
        let Some(idx) = against_text[search_from..]
            .match_indices(line)
            .map(|(offset, _)| search_from + offset)
            .find(|idx| line_boundary_match(against_text, *idx, line))
        else {
            return false;
        };
        search_from = idx + line.len();
    }
    true
}

fn line_boundary_match(text: &str, start: usize, line: &str) -> bool {
    let end = start + line.len();
    if end > text.len() {
        return false;
    }
    if &text[start..end] != line {
        return false;
    }
    let before_ok = start == 0 || text.as_bytes()[start - 1] == b'\n';
    let after_ok = end == text.len() || text.as_bytes()[end] == b'\n';
    before_ok && after_ok
}

fn mergeability_from_preview(preview: &MergePreviewJson) -> Mergeability {
    if !preview.prediction_available {
        return Mergeability::Unknown;
    }
    match preview.clean {
        Some(true) => Mergeability::Clean,
        Some(false) => Mergeability::Conflicts,
        None => Mergeability::Unknown,
    }
}

fn target_risk_from_preview(preview: &MergePreviewJson) -> TargetRisk {
    if !preview.prediction_available {
        return TargetRisk::Unknown;
    }
    match preview.clean {
        Some(_) => preview.target_risk,
        None => TargetRisk::Unknown,
    }
}

fn vcs_merge_safe_from_preview(preview: &MergePreviewJson) -> Option<bool> {
    if !preview.prediction_available {
        return None;
    }
    // A conflict-free prediction that would destroy target-side state is
    // not merge-safe, whatever `clean` says (fb-105).
    if preview_invariant_violations(preview).is_some() {
        return Some(false);
    }
    // An uncertified prediction is never merge-safe: no authoritative
    // target head backed the classification (fb-105, fail closed).
    if preview_uncertified(preview) {
        return Some(false);
    }
    preview.clean
}

/// The preview's merge-invariant violations, when the four-tree
/// classification ran and found any.
fn preview_invariant_violations(preview: &MergePreviewJson) -> Option<&[String]> {
    preview
        .invariant_violations
        .as_deref()
        .filter(|violations| !violations.is_empty())
}

/// True when the four-tree classification ran but could not be certified
/// against an authoritative target head.
fn preview_uncertified(preview: &MergePreviewJson) -> bool {
    if !preview.prediction_available {
        // No prediction claimed: the missing-prediction paths handle it.
        return false;
    }
    match preview.merge_safety.as_ref() {
        Some(safety) => !safety.certified,
        // A claimed prediction with no merge_safety block at all: nothing
        // vouched for it, so nothing may infer "certified safe" from it.
        // Fail closed (fb-105 hardening).
        None => true,
    }
}

/// Why [`preview_uncertified`] holds, for reason strings.
fn preview_uncertified_cause(preview: &MergePreviewJson) -> &'static str {
    match preview.merge_safety.as_ref() {
        Some(safety) => safety.uncertified_cause.unwrap_or("uncertified"),
        None => "merge_safety_block_missing",
    }
}

#[allow(clippy::too_many_arguments)]
fn contribution_state(
    repo: &dyn Repository,
    branch_commit_count: usize,
    changed_files: &[FileChange],
    missing_blob_paths: &[String],
    local_snapshot_missing: bool,
    branch_manifest: &Manifest,
    against_manifest: &Manifest,
    against_head_missing: bool,
) -> Result<ContributionState> {
    if local_snapshot_missing || !missing_blob_paths.is_empty() {
        return Ok(ContributionState::Unknown);
    }
    if branch_commit_count == 0 {
        return Ok(ContributionState::Empty);
    }
    if against_head_missing {
        if changed_files.is_empty() {
            return Ok(ContributionState::Unknown);
        }
        return Ok(ContributionState::Contributes);
    }
    if changed_files.is_empty() {
        return Ok(ContributionState::SupersededExact);
    }
    let (all_proven, _) =
        prove_superseded_paths(repo, changed_files, against_manifest, branch_manifest)?;
    if all_proven {
        return Ok(ContributionState::SupersededExact);
    }
    Ok(ContributionState::Contributes)
}

#[allow(clippy::too_many_arguments)]
fn recommend_action(
    branch: &str,
    contribution: ContributionState,
    mergeability: Mergeability,
    target_risk: TargetRisk,
    close_allowed: bool,
    has_merge_safety_unknown: bool,
    vcs_merge_safe: Option<bool>,
    preview: &MergePreviewJson,
) -> (RecommendedAction, String, &'static str, &'static str) {
    // A merge invariant violation trumps every other signal: merging would
    // destroy target-side state the branch never touched (fb-105). Name
    // each file and the tree evidence that produced the verdict.
    if let Some(violations) = preview_invariant_violations(preview) {
        let evidence = preview
            .merge_safety
            .as_ref()
            .map(|safety| {
                format!(
                    "fork_base={}, branch_head={}, target_head={} ({})",
                    safety.fork_base_commit.as_deref().unwrap_or("unavailable"),
                    safety.branch_head.as_deref().unwrap_or("unavailable"),
                    safety.target_head.as_deref().unwrap_or("unavailable"),
                    safety.target_head_source,
                )
            })
            .unwrap_or_else(|| "merge_safety evidence unavailable".to_string());
        return (
            RecommendedAction::DoNotMerge,
            format!(
                "merge_invariant_violation: predicted merge result destroys target-side \
                 state the branch never touched: {} (evidence: {evidence})",
                crate::commands::merge_safety::bounded_path_list(violations)
            ),
            "high",
            "merge_safety_four_tree",
        );
    }

    if contribution == ContributionState::Empty && close_allowed {
        return (
            RecommendedAction::Close,
            "empty".to_string(),
            "high",
            "lineage",
        );
    }

    if contribution == ContributionState::SupersededExact && close_allowed {
        return (
            RecommendedAction::Close,
            "superseded_exact".to_string(),
            "high",
            "tree_equality",
        );
    }

    if contribution == ContributionState::Contributes
        && target_risk == TargetRisk::RevertsTargetExact
    {
        return (
            RecommendedAction::Rebuild,
            "reverts_target_exact".to_string(),
            "high",
            "merge_projection",
        );
    }

    // Fail closed on an uncertified prediction: without an authoritative
    // target head no evidence below can justify a merge recommendation.
    // Covers both an explicit uncertified verdict and a claimed prediction
    // whose merge_safety block is absent entirely (fb-105 hardening).
    // Close/Rebuild above stay reachable — neither merges anything.
    if preview_uncertified(preview) {
        let cause = preview_uncertified_cause(preview);
        return (
            RecommendedAction::Review,
            format!(
                "merge_safety_uncertified: {cause} — the prediction could not be certified \
                 against an authoritative target head; refresh (`oak fetch` or `oak branch \
                 review --remote`) before merging"
            ),
            "low",
            "merge_safety_four_tree",
        );
    }

    if has_merge_safety_unknown {
        let reason = if !preview.prediction_available {
            "missing_merge_prediction".to_string()
        } else if contribution == ContributionState::Unknown {
            "contribution_unverified".to_string()
        } else if mergeability == Mergeability::Unknown {
            "mergeability_unknown".to_string()
        } else if target_risk == TargetRisk::Unknown {
            "target_risk_unknown".to_string()
        } else {
            "insufficient_evidence".to_string()
        };
        return (RecommendedAction::Review, reason, "low", "summary");
    }

    if contribution == ContributionState::Contributes {
        return match mergeability {
            Mergeability::Clean if vcs_merge_safe == Some(true) => (
                RecommendedAction::ValidateThenMerge,
                "clean_contribution".to_string(),
                "medium",
                "merge_prediction",
            ),
            Mergeability::Conflicts => (
                RecommendedAction::Resolve,
                "merge_conflicts".to_string(),
                "medium",
                "merge_prediction",
            ),
            Mergeability::Clean => (
                RecommendedAction::Review,
                "mergeability_unverified".to_string(),
                "low",
                "summary",
            ),
            Mergeability::Unknown => (
                RecommendedAction::Review,
                "mergeability_unknown".to_string(),
                "low",
                "summary",
            ),
        };
    }

    (
        RecommendedAction::Unknown,
        format!("unclassified_branch:{branch}"),
        "low",
        "summary",
    )
}

// --- Batch triage orchestration (uses shared engine above; no second derivation) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisDepth {
    Summary,
    Manifest,
    Full,
}

impl AnalysisDepth {
    fn wants_merge_preview(self) -> bool {
        matches!(self, Self::Manifest | Self::Full)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageOnlyFilter {
    Closable,
    Mergeable,
    Ambiguous,
}

#[derive(Debug, Serialize)]
pub struct BranchTriageRow {
    branch: String,
    branch_head: Option<String>,
    against_head: Option<String>,
    fork_point: Option<String>,
    recommended_action: RecommendedAction,
    recommended_action_detail: output::AgentRecommendedActionJson,
    reason: String,
    confidence: &'static str,
    analysis_depth: &'static str,
    analysis_budget_exhausted: bool,
    close_allowed: bool,
    vcs_merge_safe: Option<bool>,
    merge_allowed: bool,
    checks: ChecksJson,
    mergeability: Mergeability,
    contribution: ContributionState,
    target_risk: TargetRisk,
    unique_contribution: UniqueContributionJson,
    next_detail_command: String,
    missing_data: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred: Option<bool>,
    /// Set when this branch could not be assessed; other rows are
    /// unaffected (fb-118).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<TriageRowErrorJson>,
}

#[derive(Debug, Serialize)]
struct BatchTriageJson {
    schema_version: u32,
    against: String,
    remote: bool,
    branches_analyzed: usize,
    branches_deferred: usize,
    elapsed_budget_ms: Option<u64>,
    caveats: Vec<String>,
    rows: Vec<BranchTriageRow>,
}

#[allow(clippy::too_many_arguments)]
pub fn branch_triage_json(
    path: &Path,
    against: &str,
    status: Option<&str>,
    analysis_depth: AnalysisDepth,
    only: Option<TriageOnlyFilter>,
    limit: Option<usize>,
    rt: Option<&tokio::runtime::Runtime>,
) -> Result<()> {
    let started = Instant::now();
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    // One corrupt stored row must not abort the batch (fb-118): readable
    // rows are assessed normally, unreadable ones become error rows.
    let mut branches: Vec<(String, Option<String>)> = Vec::new();
    let mut error_rows = Vec::new();
    for entry in repo.list_branches_isolated()? {
        match entry {
            Ok(branch) => {
                if branch.name == against || !status_filter(status, branch.status) {
                    continue;
                }
                match repo.get_branch_head(&branch.name) {
                    Ok(head) => branches.push((branch.name, head.map(|h| h.to_string()))),
                    Err(error) => {
                        let mut row = deferred_row(
                            &branch.name,
                            None,
                            None,
                            analysis_depth,
                            false,
                            false,
                            vec!["branch_head_unreadable".to_string()],
                        );
                        let mut row_error = row_error(&branch.name, &error);
                        row_error.column = Some("head");
                        row.reason = row_error.code.to_string();
                        row.error = Some(row_error);
                        error_rows.push(row);
                    }
                }
            }
            Err(unreadable) => {
                if unreadable.name == against || !status_filter(status, unreadable.status) {
                    continue;
                }
                let head = repo
                    .get_branch_head(&unreadable.name)
                    .ok()
                    .flatten()
                    .map(|h| h.to_string());
                error_rows.push(unreadable_row(&unreadable, head, analysis_depth));
            }
        }
    }
    branches.sort_by(|a, b| a.0.cmp(&b.0));

    let mut caveats =
        vec!["Batch triage analyzed local branch metadata without switching checkout.".to_string()];
    if !analysis_depth.wants_merge_preview() {
        caveats.push(
            "Summary analysis depth skips merge prediction; vcs_merge_safe stays null.".to_string(),
        );
    }
    let ci = match rt {
        Some(rt) => rt.block_on(CiEvidence::observe_for_repo(repo.as_ref())),
        None => CiEvidence::NotQueried(CHECKS_REASON_NOT_REQUESTED),
    };
    if matches!(ci, CiEvidence::Listed { .. }) {
        caveats.push(format!(
            "Checks were read from one CI runs listing (newest {CHECKS_SCAN_LIMIT} runs) and bound to each local branch head; a local head that was never pushed has no run."
        ));
    }
    let mut rows = analyze_branch_rows(
        repo.as_ref(),
        &branches,
        against,
        false,
        analysis_depth,
        only,
        limit,
        &mut caveats,
        &ci,
    )?;
    if !error_rows.is_empty() {
        caveats.push(format!(
            "{} branch row(s) could not be read from local storage and are reported with an `error`; all other branches were assessed.",
            error_rows.len()
        ));
        rows.extend(apply_only_filter(error_rows, only));
        rows.sort_by(|a, b| a.branch.cmp(&b.branch));
    }
    attach_unassessed_checks(&mut rows, &ci);
    if rows.iter().any(|row| window_incomplete(&row.checks)) {
        caveats.push(CHECKS_WINDOW_INCOMPLETE_CAVEAT.to_string());
    }
    let (analyzed, deferred) = count_analyzed_deferred(&rows);

    output::print_json(&BatchTriageJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        against: against.to_string(),
        remote: false,
        branches_analyzed: analyzed,
        branches_deferred: deferred,
        elapsed_budget_ms: Some(started.elapsed().as_millis() as u64),
        caveats,
        rows,
    })
}

pub async fn remote_branch_triage_json(
    path: &Path,
    against: &str,
    status: Option<&str>,
    analysis_depth: AnalysisDepth,
    only: Option<TriageOnlyFilter>,
    limit: Option<usize>,
) -> Result<()> {
    let started = Instant::now();
    let workspace = RemoteWorkspace::open(path)?;
    let repo = workspace.repo;
    let remote = workspace.remote;
    let remote_branches = fetch_remote_branches(&remote).await?;
    let mut branches: Vec<(String, Option<String>)> = remote_branches
        .iter()
        .filter(|branch| branch.name != against)
        .filter(|branch| status_filter(status, BranchStatus::from_db_str(&branch.status)))
        .map(|branch| (branch.name.clone(), branch.head.clone()))
        .collect();
    branches.sort_by(|a, b| a.0.cmp(&b.0));

    let mut caveats = vec![
        "Remote branch evidence was fetched without switching checkout; local refs and branch metadata were not changed.".to_string(),
        "Batch triage uses locally available commits and manifests; run `oak fetch` when heads are missing.".to_string(),
    ];
    if !analysis_depth.wants_merge_preview() {
        caveats.push(
            "Summary analysis depth skips merge prediction; vcs_merge_safe stays null.".to_string(),
        );
    }

    let analyze_count = limit.unwrap_or(branches.len());
    let branches_to_prepare: Vec<String> = branches
        .iter()
        .take(analyze_count)
        .map(|(branch, _head)| branch.clone())
        .collect();
    let prepare_errors = prepare_remote_branches_for_triage(
        &repo,
        &remote,
        &remote_branches,
        &branches_to_prepare,
        against,
    )
    .await;
    // One runs listing for the whole batch, bound per row to the pinned
    // remote head (fb-120).
    let ci = CiEvidence::observe_for_remote(&remote).await;
    if matches!(ci, CiEvidence::Listed { .. }) {
        caveats.push(format!(
            "Checks were read from one CI runs listing (newest {CHECKS_SCAN_LIMIT} runs) and bound to each pinned remote head."
        ));
    }

    let mut rows = Vec::new();
    for (index, (branch_name, head)) in branches.iter().enumerate() {
        if index < analyze_count {
            if let Some(error) = prepare_errors.get(branch_name) {
                rows.push(deferred_row(
                    branch_name,
                    head.clone(),
                    None,
                    analysis_depth,
                    true,
                    false,
                    vec![format!("remote_prepare_failed: {error}")],
                ));
                continue;
            }
            let source = remote_analysis_branch(&remote_branches, branch_name)?;
            let target = remote_analysis_branch(&remote_branches, against)?;
            match crate::commands::review::remote_branch_triage_evidence(
                &repo,
                &source,
                &target,
                analysis_depth.wants_merge_preview(),
            ) {
                Ok((comparison, mut triage)) => {
                    let pinned = comparison.branch_head.as_ref().map(ToString::to_string);
                    apply_checks(&mut triage, ci.checks_for(branch_name, pinned.as_deref()));
                    rows.push(row_from_triage(branch_name, &comparison, triage, None))
                }
                Err(error) => rows.push(failed_row(
                    branch_name,
                    head.clone(),
                    analysis_depth,
                    true,
                    &error,
                )),
            }
        } else {
            rows.push(deferred_row(
                branch_name,
                head.clone(),
                None,
                analysis_depth,
                true,
                true,
                vec!["analysis_deferred_by_limit".to_string()],
            ));
        }
    }
    rows = apply_only_filter(rows, only);
    attach_unassessed_checks(&mut rows, &ci);
    if rows.iter().any(|row| window_incomplete(&row.checks)) {
        caveats.push(CHECKS_WINDOW_INCOMPLETE_CAVEAT.to_string());
    }
    let (analyzed, deferred) = count_analyzed_deferred(&rows);

    output::print_json(&BatchTriageJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        against: against.to_string(),
        remote: true,
        branches_analyzed: analyzed,
        branches_deferred: deferred,
        elapsed_budget_ms: Some(started.elapsed().as_millis() as u64),
        caveats,
        rows,
    })
}

#[allow(clippy::too_many_arguments)]
fn analyze_branch_rows(
    repo: &dyn Repository,
    branches: &[(String, Option<String>)],
    against: &str,
    remote: bool,
    analysis_depth: AnalysisDepth,
    only: Option<TriageOnlyFilter>,
    limit: Option<usize>,
    caveats: &mut Vec<String>,
    ci: &CiEvidence,
) -> Result<Vec<BranchTriageRow>> {
    if against != DEFAULT_BRANCH && repo.get_branch_head(against)?.is_none() {
        caveats.push(format!(
            "Against branch '{against}' has no local head; some rows may be incomplete."
        ));
    }

    let analyze_count = limit.unwrap_or(branches.len());
    let mut rows = Vec::with_capacity(branches.len());
    for (index, (branch_name, head)) in branches.iter().enumerate() {
        if index < analyze_count {
            match analyze_one_branch(
                repo,
                branch_name,
                against,
                remote,
                analysis_depth,
                false,
                ci,
            ) {
                Ok(row) => rows.push(row),
                Err(error) => rows.push(failed_row(
                    branch_name,
                    head.clone(),
                    analysis_depth,
                    remote,
                    &error,
                )),
            }
        } else {
            rows.push(deferred_row(
                branch_name,
                head.clone(),
                None,
                analysis_depth,
                remote,
                true,
                vec!["analysis_deferred_by_limit".to_string()],
            ));
        }
    }
    Ok(apply_only_filter(rows, only))
}

fn analyze_one_branch(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
    remote: bool,
    analysis_depth: AnalysisDepth,
    deferred: bool,
    ci: &CiEvidence,
) -> Result<BranchTriageRow> {
    if deferred {
        return Ok(deferred_row(
            branch,
            None,
            None,
            analysis_depth,
            remote,
            true,
            vec!["analysis_deferred_by_limit".to_string()],
        ));
    }

    let (comparison, mut triage) = branch_triage_evidence(
        repo,
        branch,
        against,
        remote,
        analysis_depth.wants_merge_preview(),
    )?;
    let head = comparison.branch_head.as_ref().map(ToString::to_string);
    apply_checks(&mut triage, ci.checks_for(branch, head.as_deref()));
    Ok(row_from_triage(branch, &comparison, triage, None))
}

fn row_from_triage(
    branch: &str,
    comparison: &BranchComparison,
    triage: BranchTriageJson,
    deferred: Option<bool>,
) -> BranchTriageRow {
    BranchTriageRow {
        branch: branch.to_string(),
        branch_head: comparison.branch_head.as_ref().map(ToString::to_string),
        against_head: comparison.against_head.as_ref().map(ToString::to_string),
        fork_point: comparison.fork_point.as_ref().map(ToString::to_string),
        recommended_action: triage.recommended_action,
        recommended_action_detail: triage.recommended_action_detail,
        reason: triage.reason,
        confidence: triage.confidence,
        analysis_depth: triage.analysis_depth,
        analysis_budget_exhausted: triage.analysis_budget_exhausted,
        close_allowed: triage.close_allowed,
        vcs_merge_safe: triage.vcs_merge_safe,
        merge_allowed: triage.merge_allowed,
        checks: triage.checks,
        mergeability: triage.mergeability,
        contribution: triage.contribution,
        target_risk: triage.target_risk,
        unique_contribution: triage.unique_contribution,
        next_detail_command: triage.next_detail_command,
        missing_data: triage.missing_data,
        deferred,
        error: None,
    }
}

fn deferred_row(
    branch: &str,
    branch_head: Option<String>,
    against_head: Option<String>,
    analysis_depth: AnalysisDepth,
    remote: bool,
    limit_deferred: bool,
    missing_data: Vec<String>,
) -> BranchTriageRow {
    let remote_flag = if remote { " --remote" } else { "" };
    let next_detail_command =
        format!("oak branch review {branch}{remote_flag} --merge-preview --json");
    BranchTriageRow {
        branch: branch.to_string(),
        branch_head,
        against_head,
        fork_point: None,
        recommended_action: RecommendedAction::Unknown,
        recommended_action_detail: recommended_action_detail(RecommendedActionDetailInput {
            branch,
            remote,
            remote_configured: remote,
            remote_degraded: remote && !limit_deferred,
            action: RecommendedAction::Unknown,
            confidence: "low",
            reason: if limit_deferred {
                "analysis_deferred"
            } else {
                "analysis_failed"
            },
            next_detail_command: &next_detail_command,
        }),
        reason: if limit_deferred {
            "analysis_deferred".to_string()
        } else {
            "analysis_failed".to_string()
        },
        confidence: "low",
        analysis_depth: if analysis_depth.wants_merge_preview() {
            "merge_prediction"
        } else {
            "summary"
        },
        analysis_budget_exhausted: limit_deferred,
        close_allowed: false,
        vcs_merge_safe: None,
        merge_allowed: false,
        checks: ChecksJson::not_queried(CHECKS_REASON_NOT_ASSESSED),
        mergeability: Mergeability::Unknown,
        contribution: ContributionState::Unknown,
        target_risk: TargetRisk::Unknown,
        unique_contribution: UniqueContributionJson {
            changed_file_count: 0,
            changed_paths_sample: Vec::new(),
        },
        next_detail_command,
        missing_data,
        deferred: Some(true),
        error: None,
    }
}

fn apply_only_filter(
    rows: Vec<BranchTriageRow>,
    only: Option<TriageOnlyFilter>,
) -> Vec<BranchTriageRow> {
    let Some(only) = only else {
        return rows;
    };
    rows.into_iter()
        .filter(|row| match only {
            TriageOnlyFilter::Closable => row.close_allowed,
            TriageOnlyFilter::Mergeable => row.vcs_merge_safe == Some(true),
            TriageOnlyFilter::Ambiguous => matches!(
                row.recommended_action,
                RecommendedAction::Review | RecommendedAction::Unknown | RecommendedAction::Resolve
            ),
        })
        .collect()
}

fn count_analyzed_deferred(rows: &[BranchTriageRow]) -> (usize, usize) {
    let deferred = rows.iter().filter(|row| row.deferred == Some(true)).count();
    (rows.len().saturating_sub(deferred), deferred)
}

fn status_filter(status: Option<&str>, branch_status: BranchStatus) -> bool {
    match status {
        Some(value) => branch_status.as_str() == value,
        None => true,
    }
}

async fn prepare_remote_branches_for_triage(
    repo: &SqliteRepository,
    remote: &RemoteIdentity,
    remote_branches: &[RemoteBranchData],
    branches: &[String],
    against: &str,
) -> HashMap<String, String> {
    let branch_heads: HashMap<String, Option<String>> = remote_branches
        .iter()
        .map(|branch| (branch.name.clone(), branch.head.clone()))
        .collect();
    let mut errors = HashMap::new();
    let mut heads = Vec::new();

    for branch in branches {
        match branch_heads.get(branch) {
            Some(Some(head)) => match oak_core::Hash::from_hex(head) {
                Ok(hash) => heads.push(hash),
                Err(error) => {
                    errors.insert(branch.clone(), error.to_string());
                }
            },
            Some(None) => {
                errors.insert(
                    branch.clone(),
                    format!("remote branch '{branch}' has no head"),
                );
            }
            None => {
                errors.insert(branch.clone(), format!("Branch not found: {branch}"));
            }
        }
    }

    if let Some(Some(head)) = branch_heads.get(against) {
        match oak_core::Hash::from_hex(head) {
            Ok(hash) => heads.push(hash),
            Err(error) => {
                for branch in branches {
                    errors
                        .entry(branch.clone())
                        .or_insert_with(|| error.to_string());
                }
            }
        }
    }

    heads.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    heads.dedup_by(|a, b| a.as_str() == b.as_str());

    let mut preparation_budget = crate::commands::blob_fetch::ReviewPreparationBudget::new();
    if !heads.is_empty() {
        if let Err(error) = crate::commands::blob_fetch::ensure_review_commits_with_budget(
            repo,
            &remote.remote_url,
            &remote.owner,
            &remote.repo_name,
            remote.token.as_deref(),
            &heads,
            &mut preparation_budget,
        )
        .await
        {
            let message = error.to_string();
            for branch in branches {
                errors
                    .entry(branch.clone())
                    .or_insert_with(|| message.clone());
            }
            return errors;
        }
    }

    let candidates: Vec<_> = branches
        .iter()
        .filter(|branch| !errors.contains_key(*branch))
        .filter_map(|branch| remote_analysis_branch(remote_branches, branch).ok())
        .collect();
    let target = match remote_analysis_branch(remote_branches, against) {
        Ok(target) => target,
        Err(error) => {
            for branch in &candidates {
                errors.insert(branch.name.clone(), error.to_string());
            }
            return errors;
        }
    };
    if let Err(error) = crate::commands::review::prepare_remote_review_bases_from_snapshots(
        repo,
        remote,
        &candidates,
        &target,
        &mut preparation_budget,
    )
    .await
    {
        for branch in &candidates {
            errors.insert(branch.name.clone(), error.to_string());
        }
    }
    errors
}

fn remote_analysis_branch(
    branches: &[RemoteBranchData],
    name: &str,
) -> Result<crate::commands::review::RemoteAnalysisBranch> {
    let branch = branches
        .iter()
        .find(|branch| branch.name == name)
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
    Ok(crate::commands::review::RemoteAnalysisBranch {
        name: branch.name.clone(),
        head: branch
            .head
            .as_deref()
            .map(oak_core::Hash::from_hex)
            .transpose()?,
        parent: branch.parent_branch.clone(),
    })
}

pub fn parse_analysis_depth(value: &str) -> Result<AnalysisDepth> {
    match value {
        "summary" => Ok(AnalysisDepth::Summary),
        "manifest" => Ok(AnalysisDepth::Manifest),
        "full" => Ok(AnalysisDepth::Full),
        other => Err(OakError::InvalidArgument(format!(
            "invalid --analysis-depth '{other}'; expected summary, manifest, or full"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_branch_triage_command(
    path: &Path,
    rt: &tokio::runtime::Runtime,
    remote: bool,
    against: &str,
    status: Option<&str>,
    analysis_depth: &str,
    only: Option<&str>,
    limit: Option<usize>,
) -> Result<()> {
    let depth = parse_analysis_depth(analysis_depth)?;
    let only_filter = only.map(parse_only_filter).transpose()?;
    if remote {
        rt.block_on(remote_branch_triage_json(
            path,
            against,
            status,
            depth,
            only_filter,
            limit,
        ))
    } else {
        branch_triage_json(path, against, status, depth, only_filter, limit, Some(rt))
    }
}

pub fn parse_only_filter(value: &str) -> Result<TriageOnlyFilter> {
    match value {
        "closable" => Ok(TriageOnlyFilter::Closable),
        "mergeable" => Ok(TriageOnlyFilter::Mergeable),
        "ambiguous" => Ok(TriageOnlyFilter::Ambiguous),
        other => Err(OakError::InvalidArgument(format!(
            "invalid --only '{other}'; expected closable, mergeable, or ambiguous"
        ))),
    }
}

// --- Exact-head CI evidence (fb-120 / fb-327) ---

pub const CHECKS_NOT_QUERIED: &str = "not_queried";
pub const CHECKS_REASON_NOT_REQUESTED: &str = "ci_query_not_requested";
pub const CHECKS_REASON_NOT_ASSESSED: &str = "branch_not_assessed";
pub const CHECKS_REASON_NO_REMOTE: &str = "no_remote_configured";
const CHECKS_BINDING: &str = "latest_run_per_workflow_for_exact_commit_on_branch";
/// One bounded runs listing serves every branch in a triage batch; the list
/// endpoint has no commit filter, so this is the window heads are matched in.
pub const CHECKS_SCAN_LIMIT: usize = 200;
const CHECKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One observation of the CI runs listing, shared by every branch assessed
/// in the same invocation (one request, not one per branch).
pub(crate) enum CiEvidence {
    /// No remote is configured (or CI was not requested): unchanged offline
    /// behaviour, reported with an explicit reason.
    NotQueried(&'static str),
    /// The listing could not be read; the reason is a bounded code.
    Unavailable(String),
    Listed {
        client: crate::commands::ci::CiClient,
        runs: Vec<crate::commands::ci::CiRun>,
    },
}

impl CiEvidence {
    /// Read the runs listing once for a local checkout. No remote → not
    /// queried; any failure → unavailable (never a guess).
    pub(crate) async fn observe_for_repo(repo: &dyn Repository) -> Self {
        match repo.get_metadata(MetadataKey::RemoteUrl) {
            Ok(Some(_)) => {}
            Ok(None) => return Self::NotQueried(CHECKS_REASON_NO_REMOTE),
            Err(_) => return Self::Unavailable("remote_metadata_unreadable".into()),
        }
        match crate::commands::ci::CiClient::from_open_repo(repo) {
            Ok(client) => Self::observe(client).await,
            Err(_) => Self::Unavailable("repository_identity_unavailable".into()),
        }
    }

    pub(crate) async fn observe_for_remote(remote: &RemoteIdentity) -> Self {
        Self::observe(crate::commands::ci::CiClient {
            remote: remote.remote_url.clone(),
            owner: remote.owner.clone(),
            repo: remote.repo_name.clone(),
            token: remote.token.clone(),
        })
        .await
    }

    pub(crate) async fn observe(client: crate::commands::ci::CiClient) -> Self {
        match tokio::time::timeout(CHECKS_TIMEOUT, client.list_runs(CHECKS_SCAN_LIMIT)).await {
            Ok(Ok(runs)) => Self::Listed { client, runs },
            Ok(Err(OakError::Http(_))) => Self::Unavailable("ci_request_failed".into()),
            Ok(Err(_)) => Self::Unavailable("ci_runs_listing_rejected_or_invalid".into()),
            Err(_) => Self::Unavailable("ci_request_timed_out".into()),
        }
    }

    /// Checks evidence for `branch` at exactly `head`, mirroring the server
    /// merge gate (newest run per workflow for the commit, any branch;
    /// completed success or skipped passes):
    /// - positive evidence comes only from runs recorded on `branch` at that
    ///   exact commit — another branch's success is never borrowed;
    /// - negative evidence is not filtered: a newest-per-workflow run for the
    ///   exact commit on another branch that failed or is still running makes
    ///   the head fail/run here too (`reason: other_branch_same_commit`),
    ///   because the server gate would refuse on it (QA-L5 H1);
    /// - the runs listing is one bounded window; a success is claimed only
    ///   when the listing is provably the whole run history, else
    ///   `unavailable`/`scan_window_incomplete` (QA-L5 M1/R2-M1). No
    ///   client-authored value (such as a commit timestamp) is trusted to
    ///   prove completeness.
    pub(crate) fn checks_for(&self, branch: &str, head: Option<&str>) -> ChecksJson {
        let mut checks = match self {
            Self::NotQueried(reason) => return ChecksJson::not_queried(reason),
            Self::Unavailable(reason) => ChecksJson {
                source: Some("unavailable".into()),
                state: "unavailable",
                reason: Some(reason.clone()),
                ..ChecksJson::not_queried(reason)
            },
            Self::Listed { .. } => ChecksJson {
                source: Some("ci_runs_listing".into()),
                state: "no_runs",
                binding: Some(CHECKS_BINDING),
                scan_limit: Some(CHECKS_SCAN_LIMIT),
                reason: None,
                ..ChecksJson::not_queried("")
            },
        };
        let Some(head) = head else {
            checks.state = "unavailable";
            checks.source = Some("unavailable".into());
            checks.reason = Some("branch_head_unavailable".into());
            return checks;
        };
        checks.head = Some(head.to_string());
        let Self::Listed { client, runs } = self else {
            return checks;
        };
        use crate::commands::ci::{current_head_run_ids, CiGateState, CiRun};
        let lookup = |ids: &[u64]| -> Vec<&CiRun> {
            ids.iter()
                .filter_map(|id| runs.iter().find(|run| run.id == *id))
                .collect()
        };
        let branch_ids = current_head_run_ids(runs, head, Some(branch));
        let branch_runs = lookup(&branch_ids);
        if branch_runs.is_empty() {
            checks.reason = Some(format!(
                "no_run_for_exact_head_in_recent_{CHECKS_SCAN_LIMIT}_runs"
            ));
            return checks;
        }
        // Newest run per workflow for the commit on any branch: the set the
        // server gate evaluates. Only its non-passing members are used.
        let gate_ids = current_head_run_ids(runs, head, None);
        let blocking_elsewhere: Vec<&CiRun> = lookup(&gate_ids)
            .into_iter()
            .filter(|run| run.branch != branch && gate_state(run) != CiGateState::Success)
            .collect();

        let decide = |candidates: &[&CiRun]| -> Option<(CiGateState, u64)> {
            for wanted in [CiGateState::Failure, CiGateState::Running] {
                if let Some(run) = candidates
                    .iter()
                    .filter(|run| gate_state(run) == wanted)
                    .max_by_key(|run| run.id)
                {
                    return Some((wanted, run.id));
                }
            }
            None
        };
        let mut run_ids = branch_ids.clone();
        let (state, decisive) = if let Some(found) = decide(&branch_runs) {
            found
        } else if let Some(found) = decide(&blocking_elsewhere) {
            checks.reason = Some("other_branch_same_commit".into());
            run_ids.extend(blocking_elsewhere.iter().map(|run| run.id));
            run_ids.sort_unstable();
            run_ids.dedup();
            found
        } else if !window_complete(runs) {
            // Every visible run passes, but an older non-passing run of
            // the same commit may sit outside the listing window.
            checks.state = "unavailable";
            checks.source = Some("unavailable".into());
            checks.reason = Some(CHECKS_WINDOW_INCOMPLETE.into());
            checks.run_ids = branch_ids;
            return checks;
        } else {
            let newest = branch_runs
                .iter()
                .map(|run| run.id)
                .max()
                .expect("non-empty");
            (CiGateState::Success, newest)
        };
        checks.state = state.as_str();
        checks.known_passed = state == CiGateState::Success;
        checks.source = Some(format!("ci_run:{decisive}@{head}"));
        checks.run_id = Some(decisive);
        checks.run_ids = run_ids;
        checks.run_url = client.run_url(decisive).ok();
        checks
    }
}

/// The server gate's pass rule: a completed success or skipped conclusion
/// passes (QA-L5 L1); everything else follows [`CiRun::gate_state`].
fn gate_state(run: &crate::commands::ci::CiRun) -> crate::commands::ci::CiGateState {
    if run.conclusion.as_deref() == Some("skipped") {
        return crate::commands::ci::CiGateState::Success;
    }
    run.gate_state()
}

/// Listings shorter than this are treated as the repository's whole run
/// history. Server dependency (QA-L5 R2-L5): oakspace `api/ci.rs` pages
/// `GET /ci/runs` newest-first with `DEFAULT_RUN_LIMIT = 50` and clamps to
/// `MAX_RUN_LIMIT = 200`; a short page is only proof of completeness while
/// the server's page cap is at least this value. A server capping below 50
/// would make every short listing look complete.
const CHECKS_COMPLETE_LISTING_BELOW: usize = 50;

/// True only when the newest-first runs listing is provably the whole run
/// history (it came back shorter than any page the server could have cut
/// it to). Busy repositories therefore cannot prove a success from the
/// listing; that needs a per-commit gate query on the server.
pub const CHECKS_WINDOW_INCOMPLETE: &str = "scan_window_incomplete";
/// Caveat attached whenever any checks result is `scan_window_incomplete`.
pub const CHECKS_WINDOW_INCOMPLETE_CAVEAT: &str = "checks.state is `unavailable` (scan_window_incomplete) where every visible run passed but the CI runs listing was a full page: the server offers no per-commit query, so a busy repository cannot prove that no older failing run of the same commit exists. Confirm with `oak ci status --json` or rely on the server merge gate until a per-commit gate query exists.";

pub(crate) fn window_incomplete(checks: &ChecksJson) -> bool {
    checks.reason.as_deref() == Some(CHECKS_WINDOW_INCOMPLETE)
}

fn window_complete(runs: &[crate::commands::ci::CiRun]) -> bool {
    runs.len() < CHECKS_COMPLETE_LISTING_BELOW.min(CHECKS_SCAN_LIMIT)
}

/// Feed exact-head evidence into an assessment. `merge_allowed` stays the
/// conjunction of the existing gates — a certified-safe merge prediction
/// that recommends merging — and passed checks; CI alone never sets it.
pub(crate) fn apply_checks(triage: &mut BranchTriageJson, checks: ChecksJson) {
    triage.merge_allowed =
        merge_allowed_from(triage.recommended_action, triage.vcs_merge_safe, &checks);
    triage.checks = checks;
}

fn merge_allowed_from(
    action: RecommendedAction,
    vcs_merge_safe: Option<bool>,
    checks: &ChecksJson,
) -> bool {
    action == RecommendedAction::ValidateThenMerge
        && vcs_merge_safe == Some(true)
        && checks.known_passed
}

/// Classify an assessment failure into a stable code and a bounded message.
/// Storage errors are summarized — their raw text can carry parser or SQL
/// internals (fb-118).
fn row_error(branch: &str, error: &OakError) -> TriageRowErrorJson {
    let message = error.to_string();
    let (code, message) = match error {
        OakError::Database(text) if text.starts_with(oak_core::BRANCH_METADATA_UNREADABLE) => {
            (oak_core::BRANCH_METADATA_UNREADABLE, message)
        }
        OakError::Database(_) => (
            "storage_unreadable",
            "local storage could not be read for this branch; diagnostics omitted".to_string(),
        ),
        _ => ("analysis_failed", message),
    };
    TriageRowErrorJson {
        code,
        branch: branch.to_string(),
        column: None,
        message,
    }
}

/// Rows that failed assessment (not budget-deferred ones) still report CI
/// evidence for their known head: checks do not depend on tree analysis.
/// `merge_allowed` stays false — no merge prediction backs these rows.
fn attach_unassessed_checks(rows: &mut [BranchTriageRow], ci: &CiEvidence) {
    for row in rows.iter_mut().filter(|row| {
        row.deferred == Some(true) && !row.analysis_budget_exhausted && row.branch_head.is_some()
    }) {
        row.checks = ci.checks_for(&row.branch, row.branch_head.as_deref());
    }
}

fn failed_row(
    branch: &str,
    branch_head: Option<String>,
    analysis_depth: AnalysisDepth,
    remote: bool,
    error: &OakError,
) -> BranchTriageRow {
    let row_error = row_error(branch, error);
    let mut row = deferred_row(
        branch,
        branch_head,
        None,
        analysis_depth,
        remote,
        false,
        vec![format!("analysis_failed: {}", row_error.message)],
    );
    row.error = Some(row_error);
    row
}

fn unreadable_row(
    unreadable: &oak_core::UnreadableBranch,
    branch_head: Option<String>,
    analysis_depth: AnalysisDepth,
) -> BranchTriageRow {
    let mut row = deferred_row(
        &unreadable.name,
        branch_head,
        None,
        analysis_depth,
        false,
        false,
        vec![format!("{}:{}", unreadable.code(), unreadable.column)],
    );
    row.reason = unreadable.code().to_string();
    row.error = Some(TriageRowErrorJson {
        code: unreadable.code(),
        branch: unreadable.name.clone(),
        column: Some(unreadable.column),
        message: format!(
            "branch '{}' has an unreadable {} value in local storage; other branches were assessed normally",
            unreadable.name, unreadable.column
        ),
    });
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_allowed_requires_every_gate_not_ci_alone() {
        let passed = ChecksJson {
            known_passed: true,
            state: "success",
            ..ChecksJson::not_queried("")
        };
        let not_passed = ChecksJson::not_queried(CHECKS_REASON_NO_REMOTE);
        let merge = RecommendedAction::ValidateThenMerge;
        assert!(merge_allowed_from(merge, Some(true), &passed));
        assert!(!merge_allowed_from(merge, Some(true), &not_passed));
        assert!(!merge_allowed_from(merge, None, &passed));
        assert!(!merge_allowed_from(merge, Some(false), &passed));
        assert!(!merge_allowed_from(
            RecommendedAction::Review,
            Some(true),
            &passed
        ));
    }

    #[test]
    fn storage_errors_are_summarized_not_dumped() {
        let error = row_error(
            "feature",
            &OakError::Database("premature end of input at column 7".into()),
        );
        assert_eq!(error.code, "storage_unreadable");
        assert!(!error.message.contains("premature"));
        let typed = oak_core::UnreadableBranch {
            name: "feature".into(),
            status: BranchStatus::Open,
            column: "created_at",
        }
        .to_error();
        assert_eq!(
            row_error("feature", &typed).code,
            oak_core::BRANCH_METADATA_UNREADABLE
        );
    }

    #[test]
    fn simple_text_hunks_detect_inclusion() {
        assert!(simple_text_hunks_included(
            "alpha\nbeta\n",
            "prefix\nalpha\nbeta\nsuffix\n"
        ));
        assert!(!simple_text_hunks_included(
            "alpha\nmissing\n",
            "alpha\nbeta\n"
        ));
    }

    #[test]
    fn remote_deferred_row_reports_degraded_freshness_and_network_review() {
        let row = deferred_row(
            "remote-feature",
            Some("abc123".to_string()),
            None,
            AnalysisDepth::Full,
            true,
            false,
            vec!["remote_prepare_failed: timeout".to_string()],
        );

        assert_eq!(
            row.recommended_action_detail.kind,
            output::AgentRecommendedActionKindJson::ReviewBranch
        );
        assert!(row.recommended_action_detail.needs_network);
        assert_eq!(row.recommended_action_detail.remote_freshness, "degraded");
    }

    #[test]
    fn remote_prepare_failure_is_not_budget_exhaustion() {
        let row = deferred_row(
            "remote-feature",
            Some("abc123".to_string()),
            None,
            AnalysisDepth::Full,
            true,
            false,
            vec!["remote_prepare_failed: timeout".to_string()],
        );

        assert_eq!(row.reason, "analysis_failed");
        assert!(!row.analysis_budget_exhausted);
        assert!(row.recommended_action_detail.needs_network);
        assert_eq!(row.recommended_action_detail.remote_freshness, "degraded");
    }

    #[test]
    fn resolve_action_detail_uses_non_destructive_switch_command() {
        let detail = recommended_action_detail(RecommendedActionDetailInput {
            branch: "remote-conflict",
            remote: true,
            remote_configured: true,
            remote_degraded: false,
            action: RecommendedAction::Resolve,
            confidence: "medium",
            reason: "merge_conflicts",
            next_detail_command:
                "oak branch review remote-conflict --remote --merge-preview --json",
        });

        assert_eq!(
            detail.kind,
            output::AgentRecommendedActionKindJson::ResolveConflict
        );
        assert_eq!(detail.command, "oak switch remote-conflict");
        assert!(!detail.command.contains("--clean"));
        assert!(detail.mutates);
        assert!(detail
            .risk_notes
            .iter()
            .any(|note| note == "switch_refuses_dirty_worktree"));
    }
}

#[cfg(test)]
mod qa_l5_probe {
    use super::*;
    use crate::commands::ci::{CiClient, CiRun};

    fn run(id: u64, branch: &str, commit: &str, wf: &str, conclusion: Option<&str>) -> CiRun {
        CiRun {
            id,
            workflow_name: wf.into(),
            workflow_path: Some(format!(".oak/workflows/{wf}.yml")),
            branch: branch.into(),
            commit_hash: commit.into(),
            status: if conclusion.is_some() {
                "completed".into()
            } else {
                "running".into()
            },
            conclusion: conclusion.map(Into::into),
            ..CiRun::default()
        }
    }

    fn listed(runs: Vec<CiRun>) -> CiEvidence {
        CiEvidence::Listed {
            client: CiClient {
                remote: "http://127.0.0.1:9".into(),
                owner: "o".into(),
                repo: "r".into(),
                token: None,
            },
            runs,
        }
    }

    /// Server gate (oakspace check_merge_gate) = newest run per workflow_path
    /// for the commit across ALL branches. A newer failing run of the same
    /// commit on another branch blocks the server merge; the client must not
    /// report known_passed=true in that case.
    #[test]
    fn qa_other_branch_newer_failure_same_commit_must_not_pass() {
        let head = "h".repeat(64);
        let ci = listed(vec![
            run(11, "other", &head, "ci", Some("failure")),
            run(10, "feature", &head, "ci", Some("success")),
        ]);
        let checks = ci.checks_for("feature", Some(&head));
        assert!(
            !checks.known_passed,
            "client says known_passed (state={}, source={:?}) while the server gate's newest \
             same-commit run for workflow `ci` is #11 failure",
            checks.state, checks.source
        );
    }

    #[test]
    fn qa_other_branch_newer_running_same_commit_must_not_pass() {
        let head = "h".repeat(64);
        let ci = listed(vec![
            run(11, "other", &head, "ci", None),
            run(10, "feature", &head, "ci", Some("success")),
        ]);
        assert!(!ci.checks_for("feature", Some(&head)).known_passed);
    }

    /// Controls that must hold (expected GREEN).
    #[test]
    fn qa_controls() {
        let head = "a".repeat(64);
        let other = "b".repeat(64);
        // Different commit never borrowed.
        let ci = listed(vec![run(5, "feature", &other, "ci", Some("success"))]);
        let c = ci.checks_for("feature", Some(&head));
        assert!(!c.known_passed);
        assert_eq!(c.state, "no_runs");
        // Rerun supersedes: older success, newer failure -> failure.
        let ci = listed(vec![
            run(6, "feature", &head, "ci", Some("failure")),
            run(5, "feature", &head, "ci", Some("success")),
        ]);
        assert!(!ci.checks_for("feature", Some(&head)).known_passed);
        // Newer success supersedes older failure.
        let ci = listed(vec![
            run(6, "feature", &head, "ci", Some("success")),
            run(5, "feature", &head, "ci", Some("failure")),
        ]);
        assert!(ci.checks_for("feature", Some(&head)).known_passed);
        // Cancelled newest -> not passed.
        let ci = listed(vec![run(7, "feature", &head, "ci", Some("cancelled"))]);
        assert!(!ci.checks_for("feature", Some(&head)).known_passed);
        // Multi-workflow: one running -> not passed; one missing is invisible.
        let ci = listed(vec![
            run(8, "feature", &head, "ci", Some("success")),
            run(9, "feature", &head, "bench", None),
        ]);
        let c = ci.checks_for("feature", Some(&head));
        assert!(!c.known_passed);
        assert_eq!(c.state, "running");
        // Other-branch-only success at same commit -> no_runs (not borrowed).
        let ci = listed(vec![run(9, "other", &head, "ci", Some("success"))]);
        let c = ci.checks_for("feature", Some(&head));
        assert!(!c.known_passed);
        assert_eq!(c.state, "no_runs");
        // Server treats `skipped` as pass; so does the client (QA-L5 L1).
        let ci = listed(vec![run(9, "feature", &head, "ci", Some("skipped"))]);
        let c = ci.checks_for("feature", Some(&head));
        assert!(c.known_passed);
        assert_eq!(c.state, "success");
    }

    #[test]
    fn other_branch_blocking_run_is_reported_with_reason() {
        let head = "h".repeat(64);
        let ci = listed(vec![
            run(11, "other", &head, "ci", Some("failure")),
            run(10, "feature", &head, "ci", Some("success")),
        ]);
        let c = ci.checks_for("feature", Some(&head));
        assert_eq!(c.state, "failure");
        assert_eq!(c.reason.as_deref(), Some("other_branch_same_commit"));
        assert_eq!(c.run_id, Some(11));
        // An older other-branch failure superseded by a newer same-workflow
        // success (on any branch) does not block, matching the server.
        let ci = listed(vec![
            run(12, "feature", &head, "ci", Some("success")),
            run(11, "other", &head, "ci", Some("failure")),
        ]);
        assert!(ci.checks_for("feature", Some(&head)).known_passed);
    }

    fn full_window(head: &str, head_queued: &str, oldest_queued: &str) -> Vec<CiRun> {
        let mut runs = vec![CiRun {
            queued_at: Some(head_queued.into()),
            ..run(1000, "feature", head, "ci", Some("success"))
        }];
        for id in (901..1000).rev() {
            runs.push(CiRun {
                queued_at: Some(oldest_queued.into()),
                ..run(id, "noise", &"c".repeat(64), "ci", Some("success"))
            });
        }
        runs
    }

    #[test]
    fn full_listing_window_is_not_proof_of_success() {
        let head = "h".repeat(64);
        // A full page: an older non-passing run of the head (e.g. a failed
        // second workflow) may sit outside the window, whatever the
        // queue times say.
        for oldest in ["2026-09-26T12:30:00Z", "2026-09-26T01:00:00Z"] {
            let ci = listed(full_window(&head, "2026-09-26T13:00:00Z", oldest));
            let c = ci.checks_for("feature", Some(&head));
            assert!(!c.known_passed);
            assert_eq!(c.state, "unavailable");
            assert_eq!(c.reason.as_deref(), Some("scan_window_incomplete"));
        }
        // A short listing is the whole history: success is provable.
        let mut runs = full_window(&head, "2026-09-26T13:00:00Z", "2026-09-26T12:30:00Z");
        runs.truncate(CHECKS_COMPLETE_LISTING_BELOW - 1);
        assert!(listed(runs).checks_for("feature", Some(&head)).known_passed);
        // A visible failure is reported whatever the window.
        let mut runs = full_window(&head, "2026-09-26T13:00:00Z", "2026-09-26T12:30:00Z");
        runs[0].conclusion = Some("failure".into());
        assert_eq!(
            listed(runs).checks_for("feature", Some(&head)).state,
            "failure"
        );
    }

    /// QA-L5 r2 probe, adapted: the commit timestamp is client-authored
    /// (clock skew, git import, forgery) and is no longer an input to the
    /// window proof at all, so a forward-dated head (claims 13:00, really
    /// 11:00 with a failing run at 11:05 outside the window, window oldest
    /// 12:30) cannot turn an incomplete window into a complete one.
    #[test]
    fn qa_r2_forward_dated_commit_must_not_prove_window() {
        let head = "h".repeat(64);
        let ci = listed(full_window(
            &head,
            "2026-09-26T13:30:00Z",
            "2026-09-26T12:30:00Z",
        ));
        let c = ci.checks_for("feature", Some(&head));
        assert!(
            !c.known_passed,
            "forward-dated committed_at proved an incomplete window complete: {c:?}"
        );
        assert_eq!(c.reason.as_deref(), Some("scan_window_incomplete"));
    }
}
