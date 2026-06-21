use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use oak_core::{
    three_way_merge_manifests, ChangeType, DiffLine, FileChange, FileDiff, FileMode, Hash,
    Manifest, ManifestEntry, MergeConflict, OakError, Repository, Result, SqliteRepository,
    DEFAULT_BRANCH,
};
use serde::Serialize;

use crate::commands::triage::{
    derive_branch_triage, BranchTriageInput, BranchTriageJson, ChecksJson, ContributionState,
    Mergeability, RecommendedAction, TargetRisk, UniqueContributionJson,
};
use crate::output;

const SCHEMA_VERSION: u32 = crate::work_state::SCHEMA_VERSION;
const MAX_STAT_BYTES: usize = 1024 * 1024;
const IDENTICAL_SAMPLE_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Tree,
    Contribution,
    NetMerge,
}

impl DiffMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "tree" => Ok(Self::Tree),
            "contribution" => Ok(Self::Contribution),
            "net-merge" => Ok(Self::NetMerge),
            _ => Err(OakError::InvalidArgument(format!(
                "unknown --diff-mode '{value}'; expected tree, contribution, or net-merge"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Contribution => "contribution",
            Self::NetMerge => "net-merge",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge_preview(clean: Option<bool>) -> ReviewMergePreviewJson {
        ReviewMergePreviewJson {
            schema_version: SCHEMA_VERSION,
            branch: "remote-feature".to_string(),
            parent: Some("main".to_string()),
            branch_head: Some("branch-head".to_string()),
            parent_head: Some("main-head".to_string()),
            prediction_available: true,
            prediction_method: "three_way_manifest",
            clean,
            conflict_file_count: if clean == Some(false) { 1 } else { 0 },
            conflict_files: if clean == Some(false) {
                vec![ConflictFileJson {
                    path: "tracked.txt".to_string(),
                    conflict_type: "content",
                }]
            } else {
                Vec::new()
            },
            changed_file_count: None,
            changed_files: None,
            changed_files_page: None,
            merge_lineage_evidence: LineageJson {
                branch_parent: Some("main".to_string()),
                branch_own_head: Some("branch-head".to_string()),
                branch_effective_head: Some("branch-head".to_string()),
                against_head: Some("main-head".to_string()),
                comparison_head: Some("main-head".to_string()),
                comparison_source: "against".to_string(),
                fork_point: Some("fork".to_string()),
                branch_commit_count: 1,
                caveats: Vec::new(),
            },
            caveats: Vec::new(),
            recommended_next_commands: Vec::new(),
        }
    }

    #[test]
    fn remote_conflict_review_recommends_non_destructive_switch() {
        assert_eq!(
            review_recommendations("remote-feature", true, Some(&merge_preview(Some(false)))),
            vec!["oak switch remote-feature", "oak pull", "oak merge"]
        );
    }

    #[test]
    fn remote_branch_diff_recommends_non_destructive_switch() {
        assert_eq!(
            branch_diff_recommendations("remote-feature", "main", true, DiffMode::Tree),
            vec![
                "oak branch diff remote-feature --remote --against main --diff-mode net-merge --json",
                "oak branch review remote-feature --remote --merge-preview --json",
                "oak switch remote-feature",
            ]
        );
    }
}

#[derive(Debug, Serialize)]
struct DiffJson {
    schema_version: u32,
    kind: &'static str,
    diff_mode: &'static str,
    branch: Option<String>,
    against: Option<String>,
    branch_head: Option<String>,
    parent: Option<String>,
    changed_file_count: usize,
    changed_files: Vec<FileSummaryJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_files_page: Option<ChangedFilesPageJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files_identical_to_against: Option<IdenticalFilesJson>,
    merge_lineage_evidence: LineageJson,
    caveats: Vec<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ChangedFilesPageJson {
    total_count: usize,
    offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
    returned_count: usize,
    omitted_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_page_command: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReviewJson {
    schema_version: u32,
    branch: String,
    against: String,
    branch_head: Option<String>,
    parent: Option<String>,
    changed_file_count: usize,
    changed_files: Vec<FileSummaryJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_files_page: Option<ChangedFilesPageJson>,
    files_identical_to_against: IdenticalFilesJson,
    merge_lineage_evidence: LineageJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_preview: Option<ReviewMergePreviewJson>,
    recommended_action: RecommendedAction,
    recommended_action_detail: output::AgentRecommendedActionJson,
    reason: String,
    confidence: &'static str,
    analysis_depth: &'static str,
    analysis_budget_exhausted: bool,
    next_detail_command: String,
    close_allowed: bool,
    vcs_merge_safe: Option<bool>,
    merge_allowed: bool,
    checks: ChecksJson,
    mergeability: Mergeability,
    contribution: ContributionState,
    target_risk: TargetRisk,
    unique_contribution: UniqueContributionJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing_data: Vec<String>,
    caveats: Vec<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MergePreviewJson {
    schema_version: u32,
    branch: String,
    parent: Option<String>,
    branch_head: Option<String>,
    parent_head: Option<String>,
    pub(crate) prediction_available: bool,
    prediction_method: &'static str,
    pub(crate) clean: Option<bool>,
    #[serde(skip_serializing)]
    pub(crate) target_risk: TargetRisk,
    conflict_file_count: usize,
    conflict_files: Vec<ConflictFileJson>,
    changed_file_count: Option<usize>,
    changed_files: Vec<FileSummaryJson>,
    merge_lineage_evidence: LineageJson,
    caveats: Vec<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ReviewMergePreviewJson {
    schema_version: u32,
    branch: String,
    parent: Option<String>,
    branch_head: Option<String>,
    parent_head: Option<String>,
    prediction_available: bool,
    prediction_method: &'static str,
    clean: Option<bool>,
    conflict_file_count: usize,
    conflict_files: Vec<ConflictFileJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_file_count: Option<Option<usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_files: Option<Vec<FileSummaryJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_files_page: Option<ChangedFilesPageJson>,
    merge_lineage_evidence: LineageJson,
    caveats: Vec<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
struct FileSummaryJson {
    path: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    #[serde(skip_serializing_if = "is_none_or_zero")]
    additions: Option<usize>,
    #[serde(skip_serializing_if = "is_none_or_zero")]
    deletions: Option<usize>,
    #[serde(skip_serializing_if = "is_false")]
    binary_or_large: bool,
    #[serde(skip_serializing_if = "is_true")]
    stats_available: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_none_or_zero(value: &Option<usize>) -> bool {
    value.is_none_or(|count| count == 0)
}

#[derive(Debug, Serialize)]
struct IdenticalFilesJson {
    available: bool,
    against: String,
    count: usize,
    files_sample: Vec<String>,
    caveats: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LineageJson {
    branch_parent: Option<String>,
    branch_own_head: Option<String>,
    branch_effective_head: Option<String>,
    against_head: Option<String>,
    comparison_head: Option<String>,
    comparison_source: String,
    fork_point: Option<String>,
    branch_commit_count: usize,
    caveats: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ConflictFileJson {
    path: String,
    conflict_type: &'static str,
}

pub(crate) struct BranchComparison {
    parent: Option<String>,
    branch_own_head: Option<Hash>,
    pub(crate) branch_head: Option<Hash>,
    pub(crate) against_head: Option<Hash>,
    comparison_head: Option<Hash>,
    comparison_source: String,
    pub(crate) fork_point: Option<Hash>,
    branch_commit_count: usize,
    branch_manifest: Manifest,
    comparison_manifest: Manifest,
    caveats: Vec<String>,
}

pub fn branch_diff_json(
    path: &Path,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    validate_changed_files_window(changed_files_limit)?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let comparison = branch_comparison(repo.as_ref(), branch, against)?;
    let computed = compute_branch_diff(repo.as_ref(), &comparison, branch, against, diff_mode)?;
    let next_command = changed_files_limit.map(|limit| {
        diff_page_command(
            branch,
            against,
            false,
            diff_mode,
            limit,
            changed_files_offset.saturating_add(limit),
        )
    });
    let (changed_files, changed_files_page) = window_changed_files(
        &computed.changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let lineage = lineage_json_for_diff(&comparison, &computed);
    let mut caveats = comparison.caveats.clone();
    caveats.extend(computed.extra_caveats);
    caveats.push(
        "No remote fetch was performed; remote main or branch state may be newer.".to_string(),
    );

    output::print_json(&DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "branch_diff",
        diff_mode: diff_mode.as_str(),
        branch: Some(branch.to_string()),
        against: Some(against.to_string()),
        branch_head: comparison.branch_head.as_ref().map(ToString::to_string),
        parent: comparison.parent.clone(),
        changed_file_count: computed.changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: computed.identical,
        merge_lineage_evidence: lineage,
        caveats,
        recommended_next_commands: branch_diff_recommendations(branch, against, false, diff_mode),
    })
}

pub async fn remote_branch_diff_json(
    path: &Path,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    validate_changed_files_window(changed_files_limit)?;
    prepare_remote_branch_review(path, branch, against).await?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let comparison = branch_comparison(repo.as_ref(), branch, against)?;
    let computed = compute_branch_diff(repo.as_ref(), &comparison, branch, against, diff_mode)?;
    let next_command = changed_files_limit.map(|limit| {
        diff_page_command(
            branch,
            against,
            true,
            diff_mode,
            limit,
            changed_files_offset.saturating_add(limit),
        )
    });
    let (changed_files, changed_files_page) = window_changed_files(
        &computed.changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let lineage = lineage_json_for_diff(&comparison, &computed);
    let mut caveats = comparison.caveats.clone();
    caveats.extend(computed.extra_caveats);
    caveats.push(
        "Remote branch metadata and manifests were refreshed without switching the checkout."
            .to_string(),
    );

    output::print_json(&DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "remote_branch_diff",
        diff_mode: diff_mode.as_str(),
        branch: Some(branch.to_string()),
        against: Some(against.to_string()),
        branch_head: comparison.branch_head.as_ref().map(ToString::to_string),
        parent: comparison.parent.clone(),
        changed_file_count: computed.changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: computed.identical,
        merge_lineage_evidence: lineage,
        caveats,
        recommended_next_commands: branch_diff_recommendations(branch, against, true, diff_mode),
    })
}

pub fn branch_review_json(
    path: &Path,
    branch: &str,
    merge_preview: bool,
    against: &str,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    validate_changed_files_window(changed_files_limit)?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let comparison = branch_comparison(repo.as_ref(), branch, against)?;
    let changes = comparison
        .comparison_manifest
        .diff(&comparison.branch_manifest);
    let changed_files_all = summarize_manifest_changes(repo.as_ref(), &changes)?;
    let changed_file_count = changed_files_all.len();
    let merge_preview_data = merge_preview_for_branch(repo.as_ref(), branch)?;
    let triage = branch_triage_json(
        repo.as_ref(),
        branch,
        false,
        &comparison,
        &changes,
        &changed_files_all,
        &merge_preview_data,
    )?;
    let preview = if merge_preview {
        let preview_next_command = changed_files_limit.map(|limit| {
            review_page_command(
                branch,
                false,
                merge_preview,
                limit,
                changed_files_offset.saturating_add(limit),
            )
        });
        Some(review_merge_preview_json(
            merge_preview_data,
            &changed_files_all,
            changed_files_limit,
            changed_files_offset,
            preview_next_command,
        ))
    } else {
        None
    };
    let next_command = changed_files_limit.map(|limit| {
        review_page_command(
            branch,
            false,
            merge_preview,
            limit,
            changed_files_offset.saturating_add(limit),
        )
    });
    let (changed_files, changed_files_page) = window_changed_files(
        &changed_files_all,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let mut caveats = comparison.caveats.clone();
    caveats.push(
        "No remote fetch was performed; remote main or branch state may be newer.".to_string(),
    );

    let recommended = review_recommendations(branch, false, preview.as_ref());

    output::print_json(&review_json(
        branch,
        against,
        &comparison,
        changed_file_count,
        changed_files,
        changed_files_page,
        preview,
        caveats,
        recommended,
        triage,
    ))
}

pub async fn remote_branch_review_json(
    path: &Path,
    branch: &str,
    merge_preview: bool,
    against: &str,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    validate_changed_files_window(changed_files_limit)?;
    prepare_remote_branch_review(path, branch, against).await?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let comparison = branch_comparison(repo.as_ref(), branch, against)?;
    let changes = comparison
        .comparison_manifest
        .diff(&comparison.branch_manifest);
    let changed_files_all = summarize_manifest_changes(repo.as_ref(), &changes)?;
    let changed_file_count = changed_files_all.len();
    let merge_preview_data = merge_preview_for_branch(repo.as_ref(), branch)?;
    let triage = branch_triage_json(
        repo.as_ref(),
        branch,
        true,
        &comparison,
        &changes,
        &changed_files_all,
        &merge_preview_data,
    )?;
    let preview = if merge_preview {
        let preview_next_command = changed_files_limit.map(|limit| {
            review_page_command(
                branch,
                true,
                merge_preview,
                limit,
                changed_files_offset.saturating_add(limit),
            )
        });
        Some(review_merge_preview_json(
            merge_preview_data,
            &changed_files_all,
            changed_files_limit,
            changed_files_offset,
            preview_next_command,
        ))
    } else {
        None
    };
    let next_command = changed_files_limit.map(|limit| {
        review_page_command(
            branch,
            true,
            merge_preview,
            limit,
            changed_files_offset.saturating_add(limit),
        )
    });
    let (changed_files, changed_files_page) = window_changed_files(
        &changed_files_all,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let mut caveats = comparison.caveats.clone();
    caveats.push(
        "Remote branch metadata and manifests were refreshed without switching the checkout."
            .to_string(),
    );

    let recommended = review_recommendations(branch, true, preview.as_ref());

    output::print_json(&review_json(
        branch,
        against,
        &comparison,
        changed_file_count,
        changed_files,
        changed_files_page,
        preview,
        caveats,
        recommended,
        triage,
    ))
}

async fn prepare_remote_branch_review(path: &Path, branch: &str, against: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let remote = crate::commands::branch::RemoteIdentity::from_repo(&repo)?;
    let mut branches = crate::commands::branch::fetch_remote_branches(&remote).await?;
    let branch_head = branches
        .iter()
        .find(|candidate| candidate.name == branch)
        .ok_or_else(|| OakError::BranchNotFound(branch.to_string()))?
        .head
        .clone()
        .ok_or_else(|| OakError::Server(format!("remote branch '{branch}' has no head")))?;
    let against_head = branches
        .iter()
        .find(|candidate| candidate.name == against)
        .and_then(|candidate| candidate.head.clone());

    let mut heads = vec![Hash::from_hex(&branch_head)?];
    if let Some(head) = against_head.as_deref() {
        heads.push(Hash::from_hex(head)?);
    }
    crate::commands::blob_fetch::ensure_commits_local(
        &repo,
        &remote.remote_url,
        &remote.owner,
        &remote.repo_name,
        remote.token.as_deref(),
        &heads,
    )
    .await?;
    crate::commands::branch::store_remote_branch_metadata(
        &repo,
        &mut branches,
        branch,
        None,
        None,
    )?;
    if against != branch && branches.iter().any(|candidate| candidate.name == against) {
        crate::commands::branch::store_remote_branch_metadata(
            &repo,
            &mut branches,
            against,
            None,
            None,
        )?;
    }
    Ok(())
}

pub fn worktree_diff_json(
    path: &Path,
    paths: &[std::path::PathBuf],
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    if changed_files_limit == Some(0) {
        return Err(OakError::InvalidArgument(
            "--changed-files-limit must be greater than 0".to_string(),
        ));
    }
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let filters = crate::commands::diff::resolve_filters(path, &ctx.work_tree, paths)?;
    let (changes, head, branch) =
        crate::commands::commit::compute_changes_for_status(repo.as_ref(), &ctx.work_tree)?;
    let changes = crate::commands::diff::filter_changes(changes, &filters);
    let changed_files = summarize_worktree_changes(repo.as_ref(), &ctx.work_tree, &changes)?;
    let changed_file_count = changed_files.len();
    let next_command = changed_files_limit.map(|limit| {
        format!(
            "oak diff --json --changed-files-limit {limit} --changed-files-offset {}",
            changed_files_offset.saturating_add(limit)
        )
    });
    let (changed_files, changed_files_page) = window_changed_files(
        &changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let dirty = changed_file_count > 0;
    let mut recommended = Vec::new();
    if changed_files_page
        .as_ref()
        .is_some_and(|page| page.omitted_count > 0)
    {
        recommended.push("oak diff --json".to_string());
    }
    if dirty {
        recommended.push("oak diff --print".to_string());
        recommended.push("oak commit".to_string());
    } else {
        recommended.push("oak status --json".to_string());
    }

    output::print_json(&DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "working_tree_diff",
        diff_mode: "working_tree",
        branch,
        against: Some("HEAD".to_string()),
        branch_head: head.as_ref().map(ToString::to_string),
        parent: None,
        changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: None,
        merge_lineage_evidence: LineageJson {
            branch_parent: None,
            branch_own_head: head.as_ref().map(ToString::to_string),
            branch_effective_head: head.as_ref().map(ToString::to_string),
            against_head: head.as_ref().map(ToString::to_string),
            comparison_head: head.as_ref().map(ToString::to_string),
            comparison_source: "working_tree_vs_head".to_string(),
            fork_point: None,
            branch_commit_count: 0,
            caveats: Vec::new(),
        },
        caveats: vec![
            "Working-tree diff was computed from local filesystem state only.".to_string(),
        ],
        recommended_next_commands: recommended,
    })
}

pub fn merge_preview_branch_json(path: &Path, branch: Option<&str>) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch_name = match branch {
        Some(name) => name.to_string(),
        None => repo
            .get_current_branch_name()?
            .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?,
    };
    let preview = merge_preview_for_branch(repo.as_ref(), &branch_name)?;
    output::print_json(&preview)
}

pub(crate) fn merge_preview_for_branch(
    repo: &dyn Repository,
    branch: &str,
) -> Result<MergePreviewJson> {
    let branch_row = repo
        .get_branch(branch)?
        .ok_or_else(|| OakError::BranchNotFound(branch.to_string()))?;
    let parent = branch_row.parent_branch.clone();
    let branch_head = crate::commands::commit::resolve_effective_head(repo, branch)?;
    let branch_manifest = manifest_for_head(repo, branch_head.as_ref())?;
    let parent_head = match parent.as_deref() {
        Some(name) => repo.get_branch_head(name)?,
        None => None,
    };
    let branch_commit_count = repo.count_commits_for_branch(branch)?;
    let comparison = branch_comparison(repo, branch, parent.as_deref().unwrap_or(DEFAULT_BRANCH))?;
    let mut caveats = comparison.caveats.clone();
    caveats.push("No remote fetch was performed; conflict prediction uses only local commits, manifests, and blobs.".to_string());

    let Some(parent_name) = parent.as_deref() else {
        return Ok(unavailable_merge_preview(
            branch,
            parent,
            branch_head,
            parent_head,
            &comparison,
            "branch has no parent",
            caveats,
        ));
    };
    let Some(parent_head_hash) = parent_head.as_ref() else {
        caveats.push(format!(
            "Parent '{parent_name}' has no local head; remote conflict prediction is unavailable."
        ));
        return Ok(unavailable_merge_preview(
            branch,
            parent,
            branch_head,
            parent_head,
            &comparison,
            "parent head unavailable locally",
            caveats,
        ));
    };

    let parent_manifest = manifest_for_head(repo, Some(parent_head_hash))?;
    let base_manifest = crate::commands::merge::find_lca_manifest_with_parent(
        repo,
        branch,
        parent_name,
        Some(parent_head_hash),
    )?;
    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);
    let target_risk =
        target_risk_from_manifest_conflicts(repo, &base_manifest, &outcome.conflicts)?;
    let conflicts =
        predict_conflicts(repo, &base_manifest, outcome.conflicts, branch, parent_name)?;
    let merged_manifest = Manifest::new(outcome.clean_entries);
    let changes = parent_manifest.diff(&merged_manifest);
    let changed_files = summarize_manifest_changes(repo, &changes)?;
    let clean = conflicts.is_empty();
    let recommended = if clean {
        vec!["oak merge".to_string()]
    } else {
        vec!["oak pull".to_string(), "oak merge".to_string()]
    };

    Ok(MergePreviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        parent,
        branch_head: branch_head.as_ref().map(ToString::to_string),
        parent_head: parent_head.as_ref().map(ToString::to_string),
        prediction_available: true,
        prediction_method: "local_manifest_three_way",
        clean: Some(clean),
        target_risk,
        conflict_file_count: conflicts.len(),
        conflict_files: conflicts,
        changed_file_count: Some(changed_files.len()),
        changed_files,
        merge_lineage_evidence: lineage_json_with_count(&comparison, branch_commit_count),
        caveats,
        recommended_next_commands: recommended,
    })
}

fn review_merge_preview_json(
    preview: MergePreviewJson,
    review_changed_files: &[FileSummaryJson],
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
    next_command: Option<String>,
) -> ReviewMergePreviewJson {
    let duplicate_changed_files = preview.changed_file_count == Some(review_changed_files.len())
        && preview.changed_files == review_changed_files;
    let (changed_files, changed_files_page) = if duplicate_changed_files {
        (None, None)
    } else {
        let (changed_files, page) = window_changed_files(
            &preview.changed_files,
            changed_files_limit,
            changed_files_offset,
            next_command,
        );
        (Some(changed_files), page)
    };
    ReviewMergePreviewJson {
        schema_version: preview.schema_version,
        branch: preview.branch,
        parent: preview.parent,
        branch_head: preview.branch_head,
        parent_head: preview.parent_head,
        prediction_available: preview.prediction_available,
        prediction_method: preview.prediction_method,
        clean: preview.clean,
        conflict_file_count: preview.conflict_file_count,
        conflict_files: preview.conflict_files,
        changed_file_count: (!duplicate_changed_files).then_some(preview.changed_file_count),
        changed_files,
        changed_files_page,
        merge_lineage_evidence: preview.merge_lineage_evidence,
        caveats: preview.caveats,
        recommended_next_commands: preview.recommended_next_commands,
    }
}

fn validate_changed_files_window(limit: Option<usize>) -> Result<()> {
    if limit == Some(0) {
        return Err(OakError::InvalidArgument(
            "--changed-files-limit must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

fn diff_page_command(
    branch: &str,
    against: &str,
    remote: bool,
    diff_mode: DiffMode,
    limit: usize,
    offset: usize,
) -> String {
    let diff_mode_flag = if diff_mode == DiffMode::Tree {
        String::new()
    } else {
        format!(" --diff-mode {}", diff_mode.as_str())
    };
    format!(
        "oak branch diff {branch}{diff_mode_flag}{} --against {against} --json --changed-files-limit {limit} --changed-files-offset {offset}",
        if remote { " --remote" } else { "" }
    )
}

fn review_page_command(
    branch: &str,
    remote: bool,
    merge_preview: bool,
    limit: usize,
    offset: usize,
) -> String {
    format!(
        "oak branch review {branch}{}{} --json --changed-files-limit {limit} --changed-files-offset {offset}",
        if remote { " --remote" } else { "" },
        if merge_preview {
            " --merge-preview"
        } else {
            ""
        }
    )
}

fn window_changed_files(
    files: &[FileSummaryJson],
    limit: Option<usize>,
    offset: usize,
    next_command: Option<String>,
) -> (Vec<FileSummaryJson>, Option<ChangedFilesPageJson>) {
    if limit.is_none() && offset == 0 {
        return (files.to_vec(), None);
    }

    let total_count = files.len();
    let start = offset.min(total_count);
    let end = limit
        .map(|limit| start.saturating_add(limit).min(total_count))
        .unwrap_or(total_count);
    let page = files[start..end].to_vec();
    let next_offset = (end < total_count).then_some(end);
    let next_page_command = next_offset.and(next_command);
    let returned_count = page.len();
    (
        page,
        Some(ChangedFilesPageJson {
            total_count,
            offset,
            limit,
            returned_count,
            omitted_count: total_count.saturating_sub(returned_count),
            next_offset,
            next_page_command,
        }),
    )
}

fn review_recommendations(
    branch: &str,
    remote: bool,
    preview: Option<&ReviewMergePreviewJson>,
) -> Vec<String> {
    let remote_flag = if remote { " --remote" } else { "" };
    let Some(preview) = preview else {
        return vec![format!(
            "oak branch review {branch}{remote_flag} --merge-preview --json"
        )];
    };

    let switch_command = format!("oak switch {branch}");
    if remote && preview.clean == Some(true) {
        return vec![format!("oak merge {branch}")];
    }
    let mut commands = vec![switch_command];
    match preview.clean {
        Some(true) => commands.push("oak merge".to_string()),
        Some(false) => {
            commands.push("oak pull".to_string());
            commands.push("oak merge".to_string());
        }
        None => commands.push("oak merge --dry-run --json".to_string()),
    }
    commands
}

fn unavailable_merge_preview(
    branch: &str,
    parent: Option<String>,
    branch_head: Option<Hash>,
    parent_head: Option<Hash>,
    comparison: &BranchComparison,
    reason: &str,
    mut caveats: Vec<String>,
) -> MergePreviewJson {
    caveats.push(reason.to_string());
    MergePreviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        parent,
        branch_head: branch_head.as_ref().map(ToString::to_string),
        parent_head: parent_head.as_ref().map(ToString::to_string),
        prediction_available: false,
        prediction_method: "local_manifest_three_way",
        clean: None,
        target_risk: TargetRisk::Unknown,
        conflict_file_count: 0,
        conflict_files: Vec::new(),
        changed_file_count: None,
        changed_files: Vec::new(),
        merge_lineage_evidence: lineage_json(comparison),
        caveats,
        recommended_next_commands: vec!["oak fetch".to_string(), "oak pull".to_string()],
    }
}

struct BranchDiffComputed {
    changed_files: Vec<FileSummaryJson>,
    changed_file_count: usize,
    identical: Option<IdenticalFilesJson>,
    lineage_comparison_head: Option<Hash>,
    lineage_comparison_source: String,
    extra_caveats: Vec<String>,
}

struct PredictedMergeAgainst {
    available: bool,
    merged_manifest: Manifest,
    conflict_count: usize,
    caveats: Vec<String>,
}

fn compute_branch_diff(
    repo: &dyn Repository,
    comparison: &BranchComparison,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
) -> Result<BranchDiffComputed> {
    match diff_mode {
        DiffMode::Tree => {
            let changes = comparison
                .comparison_manifest
                .diff(&comparison.branch_manifest);
            let changed_files = summarize_manifest_changes(repo, &changes)?;
            Ok(BranchDiffComputed {
                changed_file_count: changed_files.len(),
                changed_files,
                identical: Some(identical_files(
                    &comparison.branch_manifest,
                    &comparison.comparison_manifest,
                    against,
                )),
                lineage_comparison_head: comparison.comparison_head.clone(),
                lineage_comparison_source: comparison.comparison_source.clone(),
                extra_caveats: Vec::new(),
            })
        }
        DiffMode::Contribution => {
            let mut extra_caveats = Vec::new();
            let (fork_manifest, fork_head, source) = if let Some(fork) =
                comparison.fork_point.as_ref()
            {
                (
                    manifest_for_head(repo, Some(fork))?,
                    Some(fork.clone()),
                    "fork_point".to_string(),
                )
            } else {
                extra_caveats.push(
                        "Fork point between branch and against was unavailable; compared against the empty tree."
                            .to_string(),
                    );
                (
                    Manifest::empty(),
                    None,
                    "fork_point_unavailable_empty_tree".to_string(),
                )
            };
            let changes = fork_manifest.diff(&comparison.branch_manifest);
            let changed_files = summarize_manifest_changes(repo, &changes)?;
            Ok(BranchDiffComputed {
                changed_file_count: changed_files.len(),
                changed_files,
                identical: Some(identical_files(
                    &comparison.branch_manifest,
                    &fork_manifest,
                    against,
                )),
                lineage_comparison_head: fork_head,
                lineage_comparison_source: source,
                extra_caveats,
            })
        }
        DiffMode::NetMerge => {
            let mut extra_caveats = Vec::new();
            let Some(against_head) = comparison.against_head.as_ref() else {
                extra_caveats.push(format!(
                    "Against branch '{against}' has no local head; net-merge prediction is unavailable."
                ));
                return Ok(BranchDiffComputed {
                    changed_file_count: 0,
                    changed_files: Vec::new(),
                    identical: None,
                    lineage_comparison_head: None,
                    lineage_comparison_source: "net_merge_unavailable".to_string(),
                    extra_caveats,
                });
            };
            let against_manifest = manifest_for_head(repo, Some(against_head))?;
            let prediction = predict_merge_against(
                repo,
                branch,
                against,
                &comparison.branch_manifest,
                against_head,
                &against_manifest,
            )?;
            extra_caveats.extend(prediction.caveats);
            if !prediction.available {
                return Ok(BranchDiffComputed {
                    changed_file_count: 0,
                    changed_files: Vec::new(),
                    identical: None,
                    lineage_comparison_head: comparison.against_head.clone(),
                    lineage_comparison_source: "net_merge_unavailable".to_string(),
                    extra_caveats,
                });
            }
            let changes = against_manifest.diff(&prediction.merged_manifest);
            let changed_files = summarize_manifest_changes(repo, &changes)?;
            if prediction.conflict_count > 0 {
                extra_caveats.push(format!(
                    "Merge prediction found {} conflict file(s); net-merge diff includes only clean merge paths.",
                    prediction.conflict_count
                ));
            }
            Ok(BranchDiffComputed {
                changed_file_count: changed_files.len(),
                changed_files,
                identical: Some(identical_files(
                    &prediction.merged_manifest,
                    &against_manifest,
                    against,
                )),
                lineage_comparison_head: comparison.against_head.clone(),
                lineage_comparison_source: "predicted_merge_result".to_string(),
                extra_caveats,
            })
        }
    }
}

fn predict_merge_against(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
    branch_manifest: &Manifest,
    against_head: &Hash,
    against_manifest: &Manifest,
) -> Result<PredictedMergeAgainst> {
    let base_manifest = crate::commands::merge::find_lca_manifest_with_parent(
        repo,
        branch,
        against,
        Some(against_head),
    )?;
    let outcome = three_way_merge_manifests(&base_manifest, branch_manifest, against_manifest);
    let conflicts = predict_conflicts(repo, &base_manifest, outcome.conflicts, branch, against)?;
    Ok(PredictedMergeAgainst {
        available: true,
        merged_manifest: Manifest::new(outcome.clean_entries),
        conflict_count: conflicts.len(),
        caveats: Vec::new(),
    })
}

fn branch_diff_recommendations(
    branch: &str,
    against: &str,
    remote: bool,
    diff_mode: DiffMode,
) -> Vec<String> {
    let remote_flag = if remote { " --remote" } else { "" };
    let switch_command = format!("oak switch {branch}");
    let mut commands = vec![
        format!("oak branch review {branch}{remote_flag} --merge-preview --json"),
        switch_command,
    ];
    if diff_mode != DiffMode::Tree {
        commands.insert(
            0,
            format!(
                "oak branch diff {branch}{remote_flag} --against {against} --diff-mode tree --json"
            ),
        );
    }
    if diff_mode != DiffMode::NetMerge {
        commands.insert(
            0,
            format!(
                "oak branch diff {branch}{remote_flag} --against {against} --diff-mode net-merge --json"
            ),
        );
    }
    commands
}

fn lineage_json_for_diff(
    comparison: &BranchComparison,
    computed: &BranchDiffComputed,
) -> LineageJson {
    LineageJson {
        branch_parent: comparison.parent.clone(),
        branch_own_head: comparison.branch_own_head.as_ref().map(ToString::to_string),
        branch_effective_head: comparison.branch_head.as_ref().map(ToString::to_string),
        against_head: comparison.against_head.as_ref().map(ToString::to_string),
        comparison_head: computed
            .lineage_comparison_head
            .as_ref()
            .map(ToString::to_string),
        comparison_source: computed.lineage_comparison_source.clone(),
        fork_point: comparison.fork_point.as_ref().map(ToString::to_string),
        branch_commit_count: comparison.branch_commit_count,
        caveats: comparison.caveats.clone(),
    }
}

pub(crate) fn branch_comparison(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
) -> Result<BranchComparison> {
    let branch_row = repo
        .get_branch(branch)?
        .ok_or_else(|| OakError::BranchNotFound(branch.to_string()))?;
    let branch_own_head = repo.get_branch_head(branch)?;
    let branch_head = crate::commands::commit::resolve_effective_head(repo, branch)?;
    let branch_manifest = manifest_for_head(repo, branch_head.as_ref())?;
    let against_head = repo.get_branch_head(against)?;
    let commits = repo.get_commits_for_branch(branch)?;
    let fork_head = commits.first().and_then(|c| c.parent_hash.clone());
    let (comparison_head, comparison_source, mut caveats) = if against_head.is_some() {
        (
            against_head.clone(),
            format!("local_branch_head:{against}"),
            Vec::new(),
        )
    } else if let Some(fork) = fork_head.clone() {
        (
            Some(fork),
            "branch_first_commit_parent_fallback".to_string(),
            vec![format!(
                "Against branch '{against}' has no local head; compared against the first branch commit's parent instead."
            )],
        )
    } else {
        (
            None,
            "empty_tree_fallback".to_string(),
            vec![format!(
                "Against branch '{against}' has no local head and no branch fork point was found; compared against the empty tree."
            )],
        )
    };
    if against == DEFAULT_BRANCH && against_head.is_none() {
        caveats.push("Local main may be absent because Oak treats main as server-owned; this command does not fetch it.".to_string());
    }
    let comparison_manifest = manifest_for_head(repo, comparison_head.as_ref())?;
    let fork_point = match (branch_head.as_ref(), comparison_head.as_ref()) {
        (Some(branch_head), Some(comparison_head)) => {
            first_common_ancestor(repo, branch_head, comparison_head)?
        }
        _ => None,
    };

    Ok(BranchComparison {
        parent: branch_row.parent_branch,
        branch_own_head,
        branch_head,
        against_head,
        comparison_head,
        comparison_source,
        fork_point,
        branch_commit_count: commits.len(),
        branch_manifest,
        comparison_manifest,
        caveats,
    })
}

fn manifest_for_head(repo: &dyn Repository, head: Option<&Hash>) -> Result<Manifest> {
    let Some(head) = head else {
        return Ok(Manifest::empty());
    };
    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    Ok(repo
        .get_manifest(&commit.manifest_hash)?
        .unwrap_or_else(Manifest::empty))
}

fn summarize_manifest_changes(
    repo: &dyn Repository,
    changes: &[FileChange],
) -> Result<Vec<FileSummaryJson>> {
    changes
        .iter()
        .map(|change| {
            let old_bytes = bytes_for_hash(repo, change.old_blob_hash.as_ref())?;
            let new_bytes = bytes_for_hash(repo, change.new_blob_hash.as_ref())?;
            Ok(file_summary(
                change,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            ))
        })
        .collect()
}

fn summarize_worktree_changes(
    repo: &dyn Repository,
    work_tree: &Path,
    changes: &[FileChange],
) -> Result<Vec<FileSummaryJson>> {
    changes
        .iter()
        .map(|change| {
            let old_bytes = bytes_for_hash(repo, change.old_blob_hash.as_ref())?;
            let new_bytes = match change.change_type {
                ChangeType::Deleted => None,
                _ => Some(fs::read(work_tree.join(&change.path)).unwrap_or_default()),
            };
            Ok(file_summary(
                change,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            ))
        })
        .collect()
}

fn bytes_for_hash(repo: &dyn Repository, hash: Option<&Hash>) -> Result<Option<Vec<u8>>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    Ok(repo.get_blob(hash)?.map(|b| b.content))
}

fn file_summary(
    change: &FileChange,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
) -> FileSummaryJson {
    let missing_blob = (change.old_blob_hash.is_some() && old_bytes.is_none())
        || (change.new_blob_hash.is_some() && new_bytes.is_none());
    if missing_blob {
        return FileSummaryJson {
            path: change.path.clone(),
            status: output::change_type_name(change.change_type),
            old_path: change.old_path.clone(),
            additions: None,
            deletions: None,
            binary_or_large: true,
            stats_available: false,
        };
    }

    let (additions, deletions, binary_or_large, stats_available) =
        line_stats(old_bytes.unwrap_or(&[]), new_bytes.unwrap_or(&[]));
    FileSummaryJson {
        path: change.path.clone(),
        status: output::change_type_name(change.change_type),
        old_path: change.old_path.clone(),
        additions,
        deletions,
        binary_or_large,
        stats_available,
    }
}

fn line_stats(old_bytes: &[u8], new_bytes: &[u8]) -> (Option<usize>, Option<usize>, bool, bool) {
    if old_bytes.len() > MAX_STAT_BYTES || new_bytes.len() > MAX_STAT_BYTES {
        return (None, None, true, false);
    }
    let (Ok(old), Ok(new)) = (
        std::str::from_utf8(old_bytes),
        std::str::from_utf8(new_bytes),
    ) else {
        return (None, None, true, false);
    };
    let diff = FileDiff::new("", old, new);
    let additions = diff
        .lines
        .iter()
        .filter(|line| matches!(line, DiffLine::Added(_)))
        .count();
    let deletions = diff
        .lines
        .iter()
        .filter(|line| matches!(line, DiffLine::Removed(_)))
        .count();
    (Some(additions), Some(deletions), false, true)
}

fn identical_files(
    branch_manifest: &Manifest,
    against_manifest: &Manifest,
    against: &str,
) -> IdenticalFilesJson {
    let against_by_path: HashMap<&str, &ManifestEntry> = against_manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut files = Vec::new();
    for entry in &branch_manifest.entries {
        if let Some(other) = against_by_path.get(entry.path.as_str()) {
            if other.blob_hash == entry.blob_hash && other.mode == entry.mode {
                files.push(entry.path.clone());
            }
        }
    }
    files.sort();
    let count = files.len();
    files.truncate(IDENTICAL_SAMPLE_LIMIT);
    IdenticalFilesJson {
        available: true,
        against: against.to_string(),
        count,
        files_sample: files,
        caveats: vec![format!(
            "files_sample is capped at {IDENTICAL_SAMPLE_LIMIT} paths."
        )],
    }
}

fn predict_conflicts(
    repo: &dyn Repository,
    base_manifest: &Manifest,
    conflicts: Vec<MergeConflict>,
    branch_name: &str,
    parent_name: &str,
) -> Result<Vec<ConflictFileJson>> {
    let mut predicted = Vec::new();
    for MergeConflict {
        path,
        branch_entry,
        parent_entry,
    } in conflicts
    {
        match (branch_entry, parent_entry) {
            (Some(be), Some(pe)) => {
                let branch_bytes = bytes_for_hash(repo, Some(&be.blob_hash))?.unwrap_or_default();
                let parent_bytes = bytes_for_hash(repo, Some(&pe.blob_hash))?.unwrap_or_default();
                let base_text = base_manifest
                    .get(&path)
                    .and_then(|entry| bytes_for_hash(repo, Some(&entry.blob_hash)).ok().flatten())
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_default();
                match (
                    String::from_utf8(branch_bytes),
                    String::from_utf8(parent_bytes),
                ) {
                    (Ok(branch_text), Ok(parent_text)) => {
                        let merged = crate::commands::merge::create_conflict_content(
                            &base_text,
                            &branch_text,
                            &parent_text,
                            branch_name,
                            parent_name,
                        );
                        if crate::commands::merge::content_has_conflict_markers(&merged) {
                            predicted.push(ConflictFileJson {
                                path,
                                conflict_type: "content",
                            });
                        }
                    }
                    _ => predicted.push(ConflictFileJson {
                        path,
                        conflict_type: "binary",
                    }),
                }
            }
            (Some(_), None) | (None, Some(_)) => predicted.push(ConflictFileJson {
                path,
                conflict_type: "modify_delete",
            }),
            (None, None) => {}
        }
    }
    predicted.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(predicted)
}

fn target_risk_from_manifest_conflicts(
    repo: &dyn Repository,
    base_manifest: &Manifest,
    conflicts: &[MergeConflict],
) -> Result<TargetRisk> {
    for conflict in conflicts {
        match conflict_reverts_target(repo, base_manifest, conflict)? {
            Some(true) => return Ok(TargetRisk::RevertsTargetExact),
            Some(false) => {}
            None => return Ok(TargetRisk::Unknown),
        }
    }
    Ok(TargetRisk::None)
}

fn conflict_reverts_target(
    repo: &dyn Repository,
    base_manifest: &Manifest,
    conflict: &MergeConflict,
) -> Result<Option<bool>> {
    let base_entry = base_manifest.get(&conflict.path);
    let parent_changed = entry_changed(base_entry, conflict.parent_entry.as_ref());
    if !parent_changed {
        return Ok(Some(false));
    }

    match (
        conflict.branch_entry.as_ref(),
        conflict.parent_entry.as_ref(),
    ) {
        (Some(_), None) => Ok(Some(true)),
        (None, Some(_)) => Ok(Some(false)),
        (None, None) => Ok(Some(false)),
        (Some(branch_entry), Some(parent_entry)) => {
            if mode_change_reverted(base_entry, branch_entry.mode, parent_entry.mode) {
                return Ok(Some(true));
            }

            if branch_entry.blob_hash == parent_entry.blob_hash {
                return Ok(Some(false));
            }

            let Some(branch_bytes) = bytes_for_hash(repo, Some(&branch_entry.blob_hash))? else {
                return Ok(None);
            };
            let Some(parent_bytes) = bytes_for_hash(repo, Some(&parent_entry.blob_hash))? else {
                return Ok(None);
            };

            let (Ok(branch_text), Ok(parent_text)) = (
                String::from_utf8(branch_bytes.clone()),
                String::from_utf8(parent_bytes.clone()),
            ) else {
                return Ok(Some(true));
            };

            let base_text = match base_entry {
                Some(entry) => match bytes_for_hash(repo, Some(&entry.blob_hash))? {
                    Some(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => text,
                        Err(_) => return Ok(None),
                    },
                    None => return Ok(None),
                },
                None => String::new(),
            };
            let merged = crate::commands::merge::create_conflict_content(
                &base_text,
                &branch_text,
                &parent_text,
                "",
                "",
            );
            if crate::commands::merge::content_has_conflict_markers(&merged) {
                return Ok(None);
            }
            Ok(Some(merged == branch_text && branch_text != parent_text))
        }
    }
}

fn entry_changed(base: Option<&ManifestEntry>, entry: Option<&ManifestEntry>) -> bool {
    match (base, entry) {
        (Some(base), Some(entry)) => base.blob_hash != entry.blob_hash || base.mode != entry.mode,
        (None, None) => false,
        _ => true,
    }
}

fn mode_change_reverted(
    base_entry: Option<&ManifestEntry>,
    branch_mode: FileMode,
    parent_mode: FileMode,
) -> bool {
    let parent_changed_mode = base_entry.map(|entry| entry.mode) != Some(parent_mode);
    parent_changed_mode && branch_mode != parent_mode
}

fn first_common_ancestor(repo: &dyn Repository, left: &Hash, right: &Hash) -> Result<Option<Hash>> {
    let left_ancestors = collect_ancestors(repo, left)?;
    let right_ancestors: HashSet<String> = collect_ancestors(repo, right)?
        .into_iter()
        .map(|hash| hash.to_string())
        .collect();
    Ok(left_ancestors
        .into_iter()
        .find(|hash| right_ancestors.contains(&hash.to_string())))
}

fn collect_ancestors(repo: &dyn Repository, head: &Hash) -> Result<Vec<Hash>> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([head.clone()]);
    let mut seen = HashSet::new();
    while let Some(hash) = queue.pop_front() {
        if !seen.insert(hash.to_string()) {
            continue;
        }
        out.push(hash.clone());
        if let Some(commit) = repo.get_commit(&hash)? {
            if let Some(parent) = commit.parent_hash {
                queue.push_back(parent);
            }
            if let Some(merge_parent) = commit.merge_parent_hash {
                queue.push_back(merge_parent);
            }
        }
    }
    Ok(out)
}

fn lineage_json(comparison: &BranchComparison) -> LineageJson {
    lineage_json_with_count(comparison, comparison.branch_commit_count)
}

fn lineage_json_with_count(
    comparison: &BranchComparison,
    branch_commit_count: usize,
) -> LineageJson {
    LineageJson {
        branch_parent: comparison.parent.clone(),
        branch_own_head: comparison.branch_own_head.as_ref().map(ToString::to_string),
        branch_effective_head: comparison.branch_head.as_ref().map(ToString::to_string),
        against_head: comparison.against_head.as_ref().map(ToString::to_string),
        comparison_head: comparison.comparison_head.as_ref().map(ToString::to_string),
        comparison_source: comparison.comparison_source.clone(),
        fork_point: comparison.fork_point.as_ref().map(ToString::to_string),
        branch_commit_count,
        caveats: comparison.caveats.clone(),
    }
}

pub(crate) fn summary_merge_preview(
    branch: &str,
    comparison: &BranchComparison,
) -> MergePreviewJson {
    unavailable_merge_preview(
        branch,
        comparison.parent.clone(),
        comparison.branch_head.clone(),
        comparison.against_head.clone(),
        comparison,
        "merge prediction skipped for summary analysis depth",
        Vec::new(),
    )
}

pub(crate) fn branch_triage_evidence(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
    remote: bool,
    include_merge_preview: bool,
) -> Result<(BranchComparison, BranchTriageJson)> {
    let comparison = branch_comparison(repo, branch, against)?;
    let changes = comparison
        .comparison_manifest
        .diff(&comparison.branch_manifest);
    let changed_files = summarize_manifest_changes(repo, &changes)?;
    let merge_preview = if include_merge_preview {
        merge_preview_for_branch(repo, branch)?
    } else {
        summary_merge_preview(branch, &comparison)
    };
    let triage = branch_triage_json(
        repo,
        branch,
        remote,
        &comparison,
        &changes,
        &changed_files,
        &merge_preview,
    )?;
    Ok((comparison, triage))
}

fn branch_triage_json(
    repo: &dyn Repository,
    branch: &str,
    remote: bool,
    comparison: &BranchComparison,
    changes: &[FileChange],
    changed_files: &[FileSummaryJson],
    merge_preview: &MergePreviewJson,
) -> Result<BranchTriageJson> {
    let missing_blob_paths: Vec<String> = changed_files
        .iter()
        .filter(|file| !file.stats_available)
        .map(|file| file.path.clone())
        .collect();
    derive_branch_triage(
        repo,
        BranchTriageInput {
            branch,
            remote,
            branch_commit_count: comparison.branch_commit_count,
            changed_files: changes,
            branch_manifest: &comparison.branch_manifest,
            against_manifest: &comparison.comparison_manifest,
            missing_blob_paths: &missing_blob_paths,
            against_head_missing: comparison.against_head.is_none(),
            merge_preview,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn review_json(
    branch: &str,
    against: &str,
    comparison: &BranchComparison,
    changed_file_count: usize,
    changed_files: Vec<FileSummaryJson>,
    changed_files_page: Option<ChangedFilesPageJson>,
    merge_preview: Option<ReviewMergePreviewJson>,
    caveats: Vec<String>,
    recommended_next_commands: Vec<String>,
    triage: BranchTriageJson,
) -> ReviewJson {
    ReviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        against: against.to_string(),
        branch_head: comparison.branch_head.as_ref().map(ToString::to_string),
        parent: comparison.parent.clone(),
        changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: identical_files(
            &comparison.branch_manifest,
            &comparison.comparison_manifest,
            against,
        ),
        merge_lineage_evidence: lineage_json(comparison),
        merge_preview,
        recommended_action: triage.recommended_action,
        recommended_action_detail: triage.recommended_action_detail,
        reason: triage.reason,
        confidence: triage.confidence,
        analysis_depth: triage.analysis_depth,
        analysis_budget_exhausted: triage.analysis_budget_exhausted,
        next_detail_command: triage.next_detail_command,
        close_allowed: triage.close_allowed,
        vcs_merge_safe: triage.vcs_merge_safe,
        merge_allowed: triage.merge_allowed,
        checks: triage.checks,
        mergeability: triage.mergeability,
        contribution: triage.contribution,
        target_risk: triage.target_risk,
        unique_contribution: triage.unique_contribution,
        missing_data: triage.missing_data,
        caveats,
        recommended_next_commands,
    }
}
