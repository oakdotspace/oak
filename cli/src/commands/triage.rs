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
use crate::commands::branch::{fetch_remote_branches, RemoteBranchData, RemoteIdentity};
use crate::commands::review::{branch_triage_evidence, BranchComparison, MergePreviewJson};
use crate::output;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Close,
    ValidateThenMerge,
    Resolve,
    Rebuild,
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

#[derive(Debug, Serialize)]
pub struct ChecksJson {
    pub required: bool,
    pub known_passed: bool,
    pub source: Option<String>,
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
        || !input.missing_blob_paths.is_empty()
        || input.against_head_missing;

    let close_allowed = input.missing_blob_paths.is_empty()
        && !input.against_head_missing
        && matches!(
            contribution,
            ContributionState::Empty | ContributionState::SupersededExact
        );

    let vcs_merge_safe = if matches!(
        contribution,
        ContributionState::Empty | ContributionState::SupersededExact
    ) {
        None
    } else {
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
        merge_allowed: false,
        checks: ChecksJson {
            required: true,
            known_passed: false,
            source: None,
        },
        mergeability,
        contribution,
        target_risk,
        unique_contribution,
        missing_data,
    })
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
    preview.clean
}

fn contribution_state(
    repo: &dyn Repository,
    branch_commit_count: usize,
    changed_files: &[FileChange],
    missing_blob_paths: &[String],
    branch_manifest: &Manifest,
    against_manifest: &Manifest,
    against_head_missing: bool,
) -> Result<ContributionState> {
    if !missing_blob_paths.is_empty() {
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

pub fn branch_triage_json(
    path: &Path,
    against: &str,
    status: Option<&str>,
    analysis_depth: AnalysisDepth,
    only: Option<TriageOnlyFilter>,
    limit: Option<usize>,
) -> Result<()> {
    let started = Instant::now();
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let mut branches: Vec<(String, Option<String>)> = repo
        .list_branches()?
        .into_iter()
        .filter(|branch| branch.name != against)
        .filter(|branch| status_filter(status, branch.status))
        .map(|branch| {
            let head = repo.get_branch_head(&branch.name)?.map(|h| h.to_string());
            Ok((branch.name, head))
        })
        .collect::<Result<_>>()?;
    branches.sort_by(|a, b| a.0.cmp(&b.0));

    let mut caveats =
        vec!["Batch triage analyzed local branch metadata without switching checkout.".to_string()];
    if !analysis_depth.wants_merge_preview() {
        caveats.push(
            "Summary analysis depth skips merge prediction; vcs_merge_safe stays null.".to_string(),
        );
    }
    let rows = analyze_branch_rows(
        repo.as_ref(),
        &branches,
        against,
        false,
        analysis_depth,
        only,
        limit,
        &mut caveats,
    )?;
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
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let remote = RemoteIdentity::from_repo(&repo)?;
    let mut remote_branches = fetch_remote_branches(&remote).await?;
    let mut branches: Vec<(String, Option<String>)> = remote_branches
        .iter()
        .filter(|branch| branch.name != against)
        .filter(|branch| status_filter(status, BranchStatus::from_db_str(&branch.status)))
        .map(|branch| (branch.name.clone(), branch.head.clone()))
        .collect();
    branches.sort_by(|a, b| a.0.cmp(&b.0));

    let mut caveats = vec![
        "Remote branch metadata was fetched without switching checkout.".to_string(),
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
        &mut remote_branches,
        &branches_to_prepare,
        against,
    )
    .await;

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
            match analyze_one_branch(&repo, branch_name, against, true, analysis_depth, false) {
                Ok(row) => rows.push(row),
                Err(error) => rows.push(deferred_row(
                    branch_name,
                    head.clone(),
                    None,
                    analysis_depth,
                    true,
                    false,
                    vec![format!("analysis_failed: {error}")],
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
            match analyze_one_branch(repo, branch_name, against, remote, analysis_depth, false) {
                Ok(row) => rows.push(row),
                Err(error) => rows.push(deferred_row(
                    branch_name,
                    head.clone(),
                    None,
                    analysis_depth,
                    remote,
                    false,
                    vec![format!("analysis_failed: {error}")],
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

    let (comparison, triage) = branch_triage_evidence(
        repo,
        branch,
        against,
        remote,
        analysis_depth.wants_merge_preview(),
    )?;
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
        checks: ChecksJson {
            required: true,
            known_passed: false,
            source: None,
        },
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
    remote_branches: &mut [RemoteBranchData],
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

    if !heads.is_empty() {
        if let Err(error) = crate::commands::blob_fetch::ensure_commits_local(
            repo,
            &remote.remote_url,
            &remote.owner,
            &remote.repo_name,
            remote.token.as_deref(),
            &heads,
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

    for branch in branches {
        if errors.contains_key(branch) {
            continue;
        }
        if let Err(error) = crate::commands::branch::store_remote_branch_metadata(
            repo,
            remote_branches,
            branch,
            None,
            None,
        ) {
            errors.insert(branch.clone(), error.to_string());
        }
    }

    if branch_heads.contains_key(against) {
        if let Err(error) = crate::commands::branch::store_remote_branch_metadata(
            repo,
            remote_branches,
            against,
            None,
            None,
        ) {
            let message = error.to_string();
            for branch in branches {
                errors
                    .entry(branch.clone())
                    .or_insert_with(|| message.clone());
            }
        }
    }

    errors
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
        branch_triage_json(path, against, status, depth, only_filter, limit)
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

#[cfg(test)]
mod tests {
    use super::*;

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
