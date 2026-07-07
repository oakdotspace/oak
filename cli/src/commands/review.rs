use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use oak_core::{
    three_way_merge_manifests, ChangeType, DiffLine, FileChange, FileDiff, FileMode, Hash,
    Manifest, ManifestEntry, MergeConflict, OakError, RenameConfig, Repository, Result,
    SqliteRepository, DEFAULT_BRANCH,
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

    fn manifest_entry(repo: &dyn Repository, path: &str, content: &str) -> (ManifestEntry, Hash) {
        let blob_hash = repo.put_blob(content.as_bytes().to_vec()).unwrap();
        (
            ManifestEntry {
                path: path.to_string(),
                blob_hash: blob_hash.clone(),
                mode: FileMode::Regular,
            },
            blob_hash,
        )
    }

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
            review_recommendations(
                "remote-feature",
                true,
                Some(&merge_preview(Some(false))),
                RecommendedAction::Resolve,
                "oak switch remote-feature",
            ),
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

    #[test]
    fn merge_preview_degrades_when_parent_ancestry_is_incomplete() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_merge_parent = Hash("ab".repeat(32));
        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                Some(missing_merge_parent),
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("parent ancestry incomplete locally")),
            "expected incomplete ancestry caveat, got {:?}",
            preview.caveats
        );
        assert_eq!(
            preview.recommended_next_commands,
            vec!["oak fetch".to_string(), "oak pull".to_string()]
        );
    }

    #[test]
    fn merge_preview_degrades_when_no_verified_common_ancestor() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_root_entry, _) = manifest_entry(&repo, "feature-root.txt", "feature root\n");
        let feature_root_manifest = repo.put_manifest(vec![feature_root_entry]).unwrap();
        let feature_root = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_root_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (foreign_root_entry, _) = manifest_entry(&repo, "foreign-root.txt", "foreign root\n");
        let foreign_root_manifest = repo.put_manifest(vec![foreign_root_entry]).unwrap();
        let foreign_root = repo
            .put_commit(
                "other".to_string(),
                None,
                None,
                foreign_root_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(feature_root),
                Some(foreign_root),
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("No verified common ancestor")),
            "expected no-common-ancestor caveat, got {:?}",
            preview.caveats
        );
    }

    #[test]
    fn merge_preview_degrades_when_parent_head_commit_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        let missing_main_head = Hash("aa".repeat(32));
        repo.set_foreign_keys(false).unwrap();
        repo.set_branch_head("main", &missing_main_head).unwrap();
        repo.set_foreign_keys(true).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("snapshot is incomplete locally")),
            "expected missing snapshot caveat, got {:?}",
            preview.caveats
        );
    }

    #[test]
    fn merge_preview_degrades_when_branch_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_feature_manifest = Hash("bc".repeat(32));
        let feature_commit = oak_core::Commit::with_timestamp(
            "feature".to_string(),
            Some(main_head.clone()),
            None,
            missing_feature_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let feature_head = feature_commit.hash.clone();
        repo.store_commit(&feature_commit).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("Branch 'feature' snapshot is incomplete locally")),
            "expected missing branch snapshot caveat, got {:?}",
            preview.caveats
        );
    }

    #[test]
    fn merge_preview_degrades_when_parent_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_main_manifest = Hash("bd".repeat(32));
        let main_commit = oak_core::Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_main_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let main_head = main_commit.hash.clone();
        repo.store_commit(&main_commit).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("Parent 'main' snapshot is incomplete locally")),
            "expected missing parent snapshot caveat, got {:?}",
            preview.caveats
        );
    }

    #[test]
    fn merge_preview_degrades_when_common_ancestor_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_base_manifest = Hash("cc".repeat(32));
        let base_commit = oak_core::Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_base_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let base_head = base_commit.hash.clone();
        repo.store_commit(&base_commit).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                Some(base_head.clone()),
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let preview = merge_preview_for_branch(&repo, "feature").unwrap();
        assert!(!preview.prediction_available);
        assert_eq!(preview.clean, None);
        assert!(
            preview
                .caveats
                .iter()
                .any(|caveat| caveat.contains("manifest data is incomplete locally")),
            "expected incomplete manifest caveat, got {:?}",
            preview.caveats
        );
    }

    #[test]
    fn net_merge_diff_degrades_when_ancestry_is_incomplete() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_merge_parent = Hash("cd".repeat(32));
        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                Some(missing_merge_parent),
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::NetMerge).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "net_merge_unavailable");
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("ancestry is incomplete locally")),
            "expected incomplete ancestry caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn net_merge_diff_degrades_when_against_head_commit_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        let missing_main_head = Hash("bb".repeat(32));
        repo.set_foreign_keys(false).unwrap();
        repo.set_branch_head("main", &missing_main_head).unwrap();
        repo.set_foreign_keys(true).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::NetMerge).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "net_merge_unavailable");
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("snapshot is incomplete locally")),
            "expected missing snapshot caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn net_merge_diff_degrades_when_against_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_main_manifest = Hash("be".repeat(32));
        let main_commit = oak_core::Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_main_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let main_head = main_commit.hash.clone();
        repo.store_commit(&main_commit).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::NetMerge).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "net_merge_unavailable");
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("snapshot is incomplete locally")),
            "expected missing snapshot caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn net_merge_diff_degrades_when_branch_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_feature_manifest = Hash("bf".repeat(32));
        let feature_commit = oak_core::Commit::with_timestamp(
            "feature".to_string(),
            Some(main_head.clone()),
            None,
            missing_feature_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let feature_head = feature_commit.hash.clone();
        repo.store_commit(&feature_commit).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::NetMerge).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "net_merge_unavailable");
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("snapshot is incomplete locally")),
            "expected missing branch snapshot caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn net_merge_diff_degrades_when_no_verified_common_ancestor() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_root_entry, _) = manifest_entry(&repo, "feature-root.txt", "feature root\n");
        let feature_root_manifest = repo.put_manifest(vec![feature_root_entry]).unwrap();
        let feature_root = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_root_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (foreign_root_entry, _) = manifest_entry(&repo, "foreign-root.txt", "foreign root\n");
        let foreign_root_manifest = repo.put_manifest(vec![foreign_root_entry]).unwrap();
        let foreign_root = repo
            .put_commit(
                "other".to_string(),
                None,
                None,
                foreign_root_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(feature_root),
                Some(foreign_root),
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::NetMerge).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "net_merge_unavailable");
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("No verified common ancestor")),
            "expected no-common-ancestor caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn tree_diff_degrades_when_branch_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_feature_manifest = Hash("c1".repeat(32));
        let feature_commit = oak_core::Commit::with_timestamp(
            "feature".to_string(),
            Some(main_head.clone()),
            None,
            missing_feature_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let feature_head = feature_commit.hash.clone();
        repo.store_commit(&feature_commit).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::Tree).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "diff_unavailable");
        assert!(computed
            .identical
            .as_ref()
            .is_some_and(|json| !json.available));
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("tree diff is unavailable")),
            "expected unavailable tree caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn tree_diff_degrades_when_comparison_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_main_manifest = Hash("c2".repeat(32));
        let main_commit = oak_core::Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_main_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let main_head = main_commit.hash.clone();
        repo.store_commit(&main_commit).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed =
            compute_branch_diff(&repo, &comparison, "feature", "main", DiffMode::Tree).unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(computed.lineage_comparison_source, "diff_unavailable");
        assert!(computed
            .identical
            .as_ref()
            .is_some_and(|json| !json.available));
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("Comparison snapshot")),
            "expected unavailable comparison caveat, got {:?}",
            computed.extra_caveats
        );
    }

    /// Rename-with-edit parity: the checkout-free branch diff must report a
    /// renamed+edited file as a single `renamed` entry (with `old_path`), the
    /// way `oak log` records it and `oak diff` shows it — not as delete+add.
    #[test]
    fn tree_and_contribution_diffs_detect_rename_with_edit() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (old_entry, _) = manifest_entry(
            &repo,
            "src/old_name.rs",
            "line one\nline two\nline three\nline four\nline five\nline six\n",
        );
        let main_manifest_hash = repo.put_manifest(vec![old_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        // Renamed and edited: 5 of 6 lines shared -> similarity ≈ 0.71.
        let (new_entry, _) = manifest_entry(
            &repo,
            "src/new_name.rs",
            "line one\nline two\nline three\nline four\nline five\nline changed\n",
        );
        let feature_manifest_hash = repo.put_manifest(vec![new_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(main_head.clone()),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        for mode in [DiffMode::Tree, DiffMode::Contribution, DiffMode::NetMerge] {
            let computed =
                compute_branch_diff(&repo, &comparison, "feature", "main", mode).unwrap();
            assert_eq!(
                computed.changed_file_count,
                1,
                "{} diff should collapse the rename-with-edit into one entry: {:?}",
                mode.as_str(),
                computed.changed_files
            );
            let file = &computed.changed_files[0];
            assert_eq!(file.path, "src/new_name.rs");
            assert_eq!(file.status, "renamed");
            assert_eq!(file.old_path.as_deref(), Some("src/old_name.rs"));
        }
    }

    #[test]
    fn contribution_diff_degrades_when_fork_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_base_manifest = Hash("c3".repeat(32));
        let base_commit = oak_core::Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_base_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let base_head = base_commit.hash.clone();
        repo.store_commit(&base_commit).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                Some(base_head.clone()),
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();
        let computed = compute_branch_diff(
            &repo,
            &comparison,
            "feature",
            "main",
            DiffMode::Contribution,
        )
        .unwrap();

        assert_eq!(computed.changed_file_count, 0);
        assert_eq!(
            computed.lineage_comparison_source,
            "contribution_unavailable"
        );
        assert!(computed
            .identical
            .as_ref()
            .is_some_and(|json| !json.available));
        assert!(
            computed
                .extra_caveats
                .iter()
                .any(|caveat| caveat.contains("Fork point snapshot")),
            "expected fork snapshot caveat, got {:?}",
            computed.extra_caveats
        );
    }

    #[test]
    fn branch_review_marks_missing_branch_snapshot_as_missing_data() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest_hash = repo.put_manifest(vec![main_entry]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let missing_feature_manifest = Hash("c4".repeat(32));
        let feature_commit = oak_core::Commit::with_timestamp(
            "feature".to_string(),
            Some(main_head.clone()),
            None,
            missing_feature_manifest,
            "tester".to_string(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        let feature_head = feature_commit.hash.clone();
        repo.store_commit(&feature_commit).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let (_comparison, triage) =
            branch_triage_evidence(&repo, "feature", "main", false, false).unwrap();

        assert_eq!(triage.contribution, ContributionState::Unknown);
        assert_eq!(triage.unique_contribution.changed_file_count, 0);
        assert!(triage
            .missing_data
            .iter()
            .any(|item| item == "local_snapshot_unavailable"));
    }

    #[test]
    fn branch_comparison_does_not_report_stale_fork_when_missing_edge_can_hide_lca() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
        let base_manifest = repo.put_manifest(vec![base_entry]).unwrap();
        let base = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = repo.put_manifest(vec![feature_entry]).unwrap();
        let missing_merge_parent = Hash("d1".repeat(32));
        let feature = repo
            .put_commit(
                "feature".to_string(),
                Some(base.clone()),
                Some(missing_merge_parent.clone()),
                feature_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();

        assert_eq!(comparison.fork_point, None);
        assert!(
            comparison
                .caveats
                .iter()
                .any(|caveat| caveat.contains("Local lineage is incomplete")
                    && caveat.contains(missing_merge_parent.short())),
            "expected incomplete lineage caveat, got {:?}",
            comparison.caveats
        );
    }

    #[test]
    fn branch_comparison_does_not_report_missing_commit_as_fork_point() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let missing_parent = Hash("d2".repeat(32));
        let (main_entry, _) = manifest_entry(&repo, "main.txt", "main\n");
        let main_manifest = repo.put_manifest(vec![main_entry]).unwrap();
        let main = repo
            .put_commit(
                "main".to_string(),
                Some(missing_parent.clone()),
                None,
                main_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature = repo
            .put_commit(
                "feature".to_string(),
                Some(missing_parent.clone()),
                None,
                feature_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &feature).unwrap();

        let comparison = branch_comparison(&repo, "feature", "main").unwrap();

        assert_eq!(comparison.fork_point, None);
        assert!(
            comparison
                .caveats
                .iter()
                .any(|caveat| caveat.contains("Local lineage is incomplete")
                    && caveat.contains(missing_parent.short())),
            "expected incomplete lineage caveat, got {:?}",
            comparison.caveats
        );
    }

    #[test]
    fn classify_change_honors_generated_marker_only_in_header_lines() {
        let generated = b"// @generated by tool\nfn main() {}\n";
        assert_eq!(
            classify_change("gen.rs", None, Some(generated), false),
            Some("generated")
        );
        // Either side's header counts — a deleted generated file is still
        // generated.
        let old_side = b"# DO NOT EDIT\n";
        assert_eq!(
            classify_change("old.cfg", Some(old_side), None, false),
            Some("generated")
        );
        // Source that merely *mentions* the marker past the header must not
        // be misfiled as generated.
        let mentions =
            b"fn main() {\n\n\n\n\n    banner(\"DO NOT EDIT\"); // @generated lookalike\n}\n";
        assert_eq!(classify_change("src.rs", None, Some(mentions), false), None);
    }

    #[test]
    fn file_summary_without_hydrated_blobs_omits_content_category() {
        let change = FileChange {
            path: "src/lib.rs".to_string(),
            change_type: ChangeType::Modified,
            old_path: None,
            old_blob_hash: Some(Hash("aa".repeat(32))),
            new_blob_hash: Some(Hash("bb".repeat(32))),
            old_mode: None,
            new_mode: None,
        };
        let summary = file_summary(&change, None, None);
        assert!(summary.binary_or_large);
        assert!(!summary.stats_available);
        // The content was never readable locally; claiming "large" would be
        // a false signal. Absent category = unknown/source.
        assert_eq!(summary.category, None);

        // Path-derived categories need no content and still apply.
        let lock = FileChange {
            path: "Cargo.lock".to_string(),
            ..change
        };
        let summary = file_summary(&lock, None, None);
        assert_eq!(summary.category, Some("lockfile"));
    }

    fn omitted_summary(path: &str, patch_omitted: bool) -> FileSummaryJson {
        FileSummaryJson {
            path: path.to_string(),
            status: "modified",
            old_path: None,
            additions: None,
            deletions: None,
            binary_or_large: false,
            stats_available: true,
            category: None,
            patch: None,
            patch_omitted,
        }
    }

    #[test]
    fn omitted_patch_commands_cover_every_file_and_shell_quote_paths() {
        let files = vec![
            omitted_summary("kept.rs", false),
            omitted_summary("a.rs", true),
            omitted_summary("with space.txt", true),
        ];
        let commands = omitted_patch_commands("oak diff feature --against main", &files);
        assert_eq!(
            commands,
            vec![
                "oak diff feature --against main --json --hunks -- a.rs",
                "oak diff feature --against main --json --hunks -- 'with space.txt'",
            ]
        );
    }

    #[test]
    fn omitted_patch_commands_bound_output_with_a_pattern_note() {
        let many: Vec<FileSummaryJson> = (0..12)
            .map(|i| omitted_summary(&format!("f{i:02}.rs"), true))
            .collect();
        let commands = omitted_patch_commands("oak diff", &many);
        assert_eq!(commands.len(), OMITTED_PATCH_COMMAND_LIMIT + 1);
        assert_eq!(commands[9], "oak diff --json --hunks -- f09.rs");
        assert_eq!(
            commands[10],
            "# 2 more patch_omitted files; fetch each with: oak diff --json --hunks -- <path>"
        );
    }

    #[test]
    fn shell_quote_path_quotes_metacharacters_and_escapes_single_quotes() {
        assert_eq!(shell_quote_path("src/main.rs"), "src/main.rs");
        assert_eq!(shell_quote_path("with space.txt"), "'with space.txt'");
        assert_eq!(shell_quote_path("it's.txt"), r"'it'\''s.txt'");
        assert_eq!(shell_quote_path("a;b.txt"), "'a;b.txt'");
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
    /// True when `--hunks` was requested but at least one returned file's
    /// patch fell outside the `--max-bytes` budget (its summary carries
    /// `patch_omitted: true`).
    #[serde(skip_serializing_if = "is_false")]
    hunks_truncated: bool,
    /// Net-merge mode: paths the merge prediction says would conflict.
    /// These are *excluded* from `changed_files` (the diff covers only
    /// clean merge paths), so they are named here instead of silently
    /// disappearing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflict_files: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    conflict_file_count: usize,
    /// Omitted when the diff kind does not compute lineage (endpoint
    /// diffs): absent means "not computed", never "computed as all-zero".
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_lineage_evidence: Option<LineageJson>,
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
    /// Noise classification — omitted for ordinary source files, so a
    /// missing category means "source". Lets consumers skip lockfile or
    /// generated churn without spending anything on its content.
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    /// Unified hunks for this file (`--hunks` only), budgeted by
    /// `--max-bytes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    /// True when `--hunks` was asked for but this file's patch fell
    /// outside the `--max-bytes` budget; fetch it alone with a path
    /// filter.
    #[serde(skip_serializing_if = "is_false")]
    patch_omitted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
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
    branch_snapshot_available: bool,
    comparison_manifest: Manifest,
    comparison_snapshot_available: bool,
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
        hunks_truncated: false,
        conflict_file_count: computed.conflict_paths.len(),
        conflict_files: computed.conflict_paths,
        merge_lineage_evidence: Some(lineage),
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
        hunks_truncated: false,
        conflict_file_count: computed.conflict_paths.len(),
        conflict_files: computed.conflict_paths,
        merge_lineage_evidence: Some(lineage),
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
    let tree_evidence = tree_diff_evidence(repo.as_ref(), &comparison, branch, against)?;
    let changed_file_count = tree_evidence.changed_file_count;
    let merge_preview_data = merge_preview_for_branch(repo.as_ref(), branch)?;
    let triage = branch_triage_json(
        repo.as_ref(),
        branch,
        false,
        &comparison,
        &tree_evidence.changes,
        &tree_evidence.changed_files,
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
            &tree_evidence.changed_files,
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
        &tree_evidence.changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let mut caveats = comparison.caveats.clone();
    caveats.extend(tree_evidence.extra_caveats.clone());
    caveats.push(
        "No remote fetch was performed; remote main or branch state may be newer.".to_string(),
    );

    let recommended = review_recommendations(
        branch,
        false,
        preview.as_ref(),
        triage.recommended_action,
        &triage.recommended_action_detail.command,
    );

    output::print_json(&review_json(
        branch,
        against,
        &comparison,
        changed_file_count,
        changed_files,
        changed_files_page,
        tree_evidence.identical,
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
    let tree_evidence = tree_diff_evidence(repo.as_ref(), &comparison, branch, against)?;
    let changed_file_count = tree_evidence.changed_file_count;
    let merge_preview_data = merge_preview_for_branch(repo.as_ref(), branch)?;
    let triage = branch_triage_json(
        repo.as_ref(),
        branch,
        true,
        &comparison,
        &tree_evidence.changes,
        &tree_evidence.changed_files,
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
            &tree_evidence.changed_files,
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
        &tree_evidence.changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let mut caveats = comparison.caveats.clone();
    caveats.extend(tree_evidence.extra_caveats.clone());
    caveats.push(
        "Remote branch metadata and manifests were refreshed without switching the checkout."
            .to_string(),
    );

    let recommended = review_recommendations(
        branch,
        true,
        preview.as_ref(),
        triage.recommended_action,
        &triage.recommended_action_detail.command,
    );

    output::print_json(&review_json(
        branch,
        against,
        &comparison,
        changed_file_count,
        changed_files,
        changed_files_page,
        tree_evidence.identical,
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

/// Machine-readable endpoint diff — the `--json` twin of `oak diff <rev>
/// [<rev>]`. Resolves the same manifest pairs the human renderers use
/// ([`branch_mode_manifests`] / endpoint manifests), so JSON and hunks can
/// never disagree about what changed. Returns whether differences were
/// found, for `--exit-code`.
/// Windowing and hunk options shared by the `oak diff --json` emitters.
#[derive(Debug, Clone, Copy)]
pub struct DiffJsonOptions {
    pub changed_files_limit: Option<usize>,
    pub changed_files_offset: usize,
    /// Include unified hunks per returned file.
    pub hunks: bool,
    /// Byte budget across all returned patches; once exceeded, remaining
    /// files carry `patch_omitted: true`.
    pub max_bytes: Option<usize>,
    /// `-U/--unified` context lines for `--hunks` patches. Honoring this
    /// matters for budgets: `-U 0` is how a caller squeezes more files
    /// under `--max-bytes`.
    pub context: usize,
}

impl Default for DiffJsonOptions {
    fn default() -> Self {
        Self {
            changed_files_limit: None,
            changed_files_offset: 0,
            hunks: false,
            max_bytes: None,
            context: oak_core::DEFAULT_CONTEXT_LINES,
        }
    }
}

pub(crate) fn endpoint_diff_json(
    path: &Path,
    endpoints: &[crate::commands::diff::Endpoint],
    paths: &[std::path::PathBuf],
    against: Option<&str>,
    mode: Option<DiffMode>,
    options: DiffJsonOptions,
) -> Result<bool> {
    use crate::commands::diff::Endpoint;

    let DiffJsonOptions {
        changed_files_limit,
        changed_files_offset,
        hunks,
        max_bytes,
        context,
    } = options;
    validate_changed_files_window(changed_files_limit)?;
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let filters = crate::commands::diff::resolve_filters(path, &ctx.work_tree, paths)?;

    // Resolve endpoints to (label pair, mode label, change set, base
    // manifest, caveats, command stub), mirroring `diff::endpoint_source`
    // case by case. The stub spells the canonical invocation with defaults
    // resolved (explicit --against/--mode), matching the human footer.
    let (
        base_label,
        target_label,
        mode_label,
        changes,
        base_manifest,
        caveats,
        conflict_paths,
        stub,
    ) = match endpoints {
        [Endpoint::Branch(branch)] => {
            let against = match against {
                Some(name) => name.to_string(),
                None => repo
                    .get_branch(branch)?
                    .and_then(|row| row.parent_branch)
                    .unwrap_or_else(|| oak_core::DEFAULT_BRANCH.to_string()),
            };
            let mode = mode.unwrap_or(DiffMode::Contribution);
            let data = branch_mode_manifests(repo.as_ref(), branch, &against, mode)?;
            let changes =
                crate::commands::diff::filter_changes(data.changes(repo.as_ref())?, &filters);
            let stub = endpoint_command_stub(endpoints, Some(&against), Some(mode));
            (
                against,
                branch.clone(),
                mode.as_str(),
                changes,
                data.base,
                data.caveats,
                data.conflict_paths,
                stub,
            )
        }
        [Endpoint::Commit(hash)] => {
            if against.is_some() || mode.is_some() {
                return Err(OakError::InvalidArgument(
                    "--against/--mode apply to branch endpoints; `oak diff <commit>` always compares the commit to the working tree".to_string(),
                ));
            }
            let base_manifest = manifest_for_head(repo.as_ref(), Some(hash))?;
            let (changes, _blobs) = crate::commands::commit::compute_worktree_changes_vs_manifest(
                repo.as_ref(),
                &ctx.work_tree,
                &base_manifest,
            )?;
            let changes = crate::commands::diff::filter_changes(changes, &filters);
            (
                Endpoint::Commit(hash.clone()).display(),
                "working_tree".to_string(),
                "commit_vs_working_tree",
                changes,
                base_manifest,
                Vec::new(),
                Vec::new(),
                endpoint_command_stub(endpoints, None, None),
            )
        }
        [base, target] => {
            if against.is_some() {
                return Err(OakError::InvalidArgument(
                    "--against conflicts with two explicit revisions; the first revision is the base".to_string(),
                ));
            }
            let (base_manifest, changes, mode_label, caveats, conflict_paths, stub) = if let (
                Endpoint::Branch(base_branch),
                Endpoint::Branch(target_branch),
            ) =
                (base, target)
            {
                let mode = mode.unwrap_or(DiffMode::Tree);
                let data = branch_mode_manifests(repo.as_ref(), target_branch, base_branch, mode)?;
                let changes = data.changes(repo.as_ref())?;
                (
                    data.base,
                    changes,
                    mode.as_str(),
                    data.caveats,
                    data.conflict_paths,
                    endpoint_command_stub(endpoints, None, Some(mode)),
                )
            } else {
                if mode.is_some_and(|m| m != DiffMode::Tree) {
                    return Err(OakError::InvalidArgument(
                            "--mode contribution/net-merge needs two branch endpoints; commit pairs always use a tree diff".to_string(),
                        ));
                }
                let base_manifest = endpoint_manifest(repo.as_ref(), base)?;
                let target_manifest = endpoint_manifest(repo.as_ref(), target)?;
                let changes = diff_manifests_with_content_renames(
                    repo.as_ref(),
                    &base_manifest,
                    &target_manifest,
                )?;
                (
                    base_manifest,
                    changes,
                    "tree",
                    Vec::new(),
                    Vec::new(),
                    endpoint_command_stub(endpoints, None, None),
                )
            };
            let changes = crate::commands::diff::filter_changes(changes, &filters);
            (
                base.display(),
                target.display(),
                mode_label,
                changes,
                base_manifest,
                caveats,
                conflict_paths,
                stub,
            )
        }
        _ => {
            return Err(OakError::InvalidArgument(
                "oak diff accepts at most two revisions".to_string(),
            ))
        }
    };

    // Scope conflicts to the user's path filters, like every change row.
    let conflict_paths: Vec<String> = conflict_paths
        .into_iter()
        .filter(|path| filters.is_empty() || crate::commands::diff::path_matches(path, &filters))
        .collect();

    let worktree_side = matches!(endpoints, [Endpoint::Commit(_)]);
    let changed_files = if worktree_side {
        summarize_worktree_changes(repo.as_ref(), &ctx.work_tree, &changes)?
    } else {
        summarize_manifest_changes(repo.as_ref(), &changes)?
    };

    let changed_file_count = changed_files.len();
    let next_command = changed_files_limit.map(|limit| {
        format!(
            "{stub} --json --changed-files-limit {limit} --changed-files-offset {}",
            changed_files_offset.saturating_add(limit)
        )
    });
    let (mut changed_files, changed_files_page) = window_changed_files(
        &changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let hunks_truncated = if hunks {
        attach_patches(
            repo.as_ref(),
            &ctx.work_tree,
            &Some(base_manifest),
            &changes,
            &mut changed_files,
            max_bytes,
            context,
        )?
    } else {
        false
    };
    let mut caveats = caveats;
    caveats.push("No remote fetch was performed; local branch state may be behind.".to_string());

    let conflict_file_count = conflict_paths.len();
    let mut recommended_next_commands = vec![format!("{stub} --print"), format!("{stub} --stat")];
    if !hunks {
        recommended_next_commands.insert(0, format!("{stub} --json --hunks"));
    }
    if hunks_truncated {
        for command in omitted_patch_commands(&stub, &changed_files)
            .into_iter()
            .rev()
        {
            recommended_next_commands.insert(0, command);
        }
    }

    output::print_json(&DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "endpoint_diff",
        diff_mode: mode_label,
        branch: Some(target_label),
        against: Some(base_label),
        branch_head: None,
        parent: None,
        changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: None,
        hunks_truncated,
        conflict_file_count,
        conflict_files: conflict_paths,
        // Endpoint diffs do not compute lineage; omitting the field is the
        // honest shape (an all-null stub with branch_commit_count 0 would
        // be indistinguishable from real zeros).
        merge_lineage_evidence: None,
        caveats,
        recommended_next_commands,
    })?;
    // Predicted conflicts are differences too: a conflicts-only net-merge
    // diff must not exit 0 as if the branches were merge-identical.
    Ok(changed_file_count > 0 || conflict_file_count > 0)
}

/// Fill `patch` on each windowed file summary from the same
/// [`crate::commands::diff::render_change_lines_with_context`] pipeline the human
/// renderers use, under an optional byte budget.
///
/// Budget semantics are strict-prefix: files are patched in order until one
/// would overflow `max_bytes`; that file and everything after it get
/// `patch_omitted: true`. Deterministic, so a caller can resume with a path
/// filter or the page cursor. Returns whether anything was omitted.
#[allow(clippy::too_many_arguments)] // internal seam with two call sites in this file
fn attach_patches(
    repo: &dyn Repository,
    work_tree: &Path,
    base_manifest: &Option<Manifest>,
    changes: &[FileChange],
    summaries: &mut [FileSummaryJson],
    max_bytes: Option<usize>,
    context: usize,
) -> Result<bool> {
    let base_files: HashMap<&str, &ManifestEntry> = base_manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry))
                .collect()
        })
        .unwrap_or_default();
    let by_path: HashMap<&str, &FileChange> = changes
        .iter()
        .map(|change| (change.path.as_str(), change))
        .collect();

    let mut used = 0usize;
    let mut truncated = false;
    for summary in summaries.iter_mut() {
        let Some(change) = by_path.get(summary.path.as_str()) else {
            continue;
        };
        if truncated {
            summary.patch_omitted = true;
            continue;
        }
        let lines = crate::commands::diff::render_change_lines_with_context(
            repo,
            work_tree,
            &base_files,
            change,
            context,
        )?;
        let mut patch = lines.join("\n");
        patch.push('\n');
        if max_bytes.is_some_and(|budget| used + patch.len() > budget) {
            truncated = true;
            summary.patch_omitted = true;
            continue;
        }
        used += patch.len();
        summary.patch = Some(patch);
    }
    Ok(truncated)
}

/// How many per-file patch-fetch commands a truncated `--hunks` payload
/// names before switching to a single pattern note — bounded so a huge
/// truncation stays token-frugal while every path is still fetchable.
const OMITTED_PATCH_COMMAND_LIMIT: usize = 10;

/// Quote a path for embedding in a recommended shell command: bare when it
/// is plainly safe, single-quoted (with embedded quotes escaped the POSIX
/// way) when it carries spaces or shell metacharacters.
fn shell_quote_path(path: &str) -> String {
    let plain = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'));
    if plain {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// One ready-to-run fetch command per `patch_omitted` file (first
/// [`OMITTED_PATCH_COMMAND_LIMIT`] of them, plus a pattern note when more
/// were omitted) — deterministic, so a caller can drain the budget
/// mechanically instead of re-deriving flags per file.
fn omitted_patch_commands(stub: &str, changed_files: &[FileSummaryJson]) -> Vec<String> {
    let omitted: Vec<&str> = changed_files
        .iter()
        .filter(|file| file.patch_omitted)
        .map(|file| file.path.as_str())
        .collect();
    let mut commands: Vec<String> = omitted
        .iter()
        .take(OMITTED_PATCH_COMMAND_LIMIT)
        .map(|path| format!("{stub} --json --hunks -- {}", shell_quote_path(path)))
        .collect();
    if omitted.len() > OMITTED_PATCH_COMMAND_LIMIT {
        commands.push(format!(
            "# {} more patch_omitted files; fetch each with: {stub} --json --hunks -- <path>",
            omitted.len() - OMITTED_PATCH_COMMAND_LIMIT
        ));
    }
    commands
}

/// The exact `oak diff …` invocation (minus output flags) that reproduces
/// an endpoint diff — used to build self-describing next commands.
pub(crate) fn endpoint_command_stub(
    endpoints: &[crate::commands::diff::Endpoint],
    against: Option<&str>,
    mode: Option<DiffMode>,
) -> String {
    let mut stub = String::from("oak diff");
    for endpoint in endpoints {
        stub.push(' ');
        stub.push_str(&endpoint.display());
    }
    if let Some(against) = against {
        stub.push_str(&format!(" --against {against}"));
    }
    if let Some(mode) = mode {
        stub.push_str(&format!(" --mode {}", mode.as_str()));
    }
    stub
}

fn endpoint_manifest(
    repo: &dyn Repository,
    endpoint: &crate::commands::diff::Endpoint,
) -> Result<Manifest> {
    use crate::commands::diff::Endpoint;
    match endpoint {
        Endpoint::Branch(name) => {
            let head = crate::commands::commit::resolve_effective_head(repo, name)?;
            manifest_for_head(repo, head.as_ref())
        }
        Endpoint::Commit(hash) => manifest_for_head(repo, Some(hash)),
    }
}

pub fn worktree_diff_json(
    path: &Path,
    paths: &[std::path::PathBuf],
    options: DiffJsonOptions,
) -> Result<bool> {
    let DiffJsonOptions {
        changed_files_limit,
        changed_files_offset,
        hunks,
        max_bytes,
        context,
    } = options;
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
    let (mut changed_files, changed_files_page) = window_changed_files(
        &changed_files,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let hunks_truncated = if hunks {
        let base_manifest = match head.as_ref() {
            Some(head) => Some(manifest_for_head(repo.as_ref(), Some(head))?),
            None => None,
        };
        attach_patches(
            repo.as_ref(),
            &ctx.work_tree,
            &base_manifest,
            &changes,
            &mut changed_files,
            max_bytes,
            context,
        )?
    } else {
        false
    };
    let dirty = changed_file_count > 0;
    let mut recommended = Vec::new();
    if changed_files_page
        .as_ref()
        .is_some_and(|page| page.omitted_count > 0)
    {
        recommended.push("oak diff --json".to_string());
    }
    if hunks_truncated {
        recommended.extend(omitted_patch_commands("oak diff", &changed_files));
    }
    if dirty {
        if !hunks {
            recommended.push("oak diff --json --hunks".to_string());
        }
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
        hunks_truncated,
        conflict_file_count: 0,
        conflict_files: Vec::new(),
        merge_lineage_evidence: Some(LineageJson {
            branch_parent: None,
            branch_own_head: head.as_ref().map(ToString::to_string),
            branch_effective_head: head.as_ref().map(ToString::to_string),
            against_head: head.as_ref().map(ToString::to_string),
            comparison_head: head.as_ref().map(ToString::to_string),
            comparison_source: "working_tree_vs_head".to_string(),
            fork_point: None,
            branch_commit_count: 0,
            caveats: Vec::new(),
        }),
        caveats: vec![
            "Working-tree diff was computed from local filesystem state only.".to_string(),
        ],
        recommended_next_commands: recommended,
    })?;
    Ok(dirty)
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
    let branch_manifest_result = manifest_for_head(repo, branch_head.as_ref());
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
    let branch_manifest = match branch_manifest_result {
        Ok(manifest) => manifest,
        Err(err) if is_local_merge_data_unavailable(&err) => {
            caveats.push(format!(
                "Branch '{branch}' snapshot is incomplete locally; remote conflict prediction is unavailable."
            ));
            return Ok(unavailable_merge_preview(
                branch,
                parent,
                branch_head,
                parent_head,
                &comparison,
                "branch snapshot unavailable locally",
                caveats,
            ));
        }
        Err(err) => return Err(err),
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

    let base_manifest = match crate::commands::merge::resolve_merge_base(
        repo,
        branch,
        parent_name,
        Some(parent_head_hash),
    ) {
        Ok(
            crate::commands::merge::MergeBaseResolution::CommonAncestor(base_manifest)
            | crate::commands::merge::MergeBaseResolution::DeclaredRoot(base_manifest),
        ) => base_manifest,
        Ok(crate::commands::merge::MergeBaseResolution::Unavailable(reason)) => {
            caveats.push(merge_base_unavailable_caveat(parent_name, &reason));
            return Ok(unavailable_merge_preview(
                branch,
                parent,
                branch_head,
                parent_head,
                &comparison,
                merge_base_unavailable_reason(&reason),
                caveats,
            ));
        }
        Err(err) => return Err(err),
    };
    let parent_manifest = match manifest_for_head(repo, Some(parent_head_hash)) {
        Ok(manifest) => manifest,
        Err(err) if is_local_merge_data_unavailable(&err) => {
            caveats.push(format!(
                "Parent '{parent_name}' snapshot is incomplete locally; remote conflict prediction is unavailable."
            ));
            return Ok(unavailable_merge_preview(
                branch,
                parent,
                branch_head,
                parent_head,
                &comparison,
                "parent snapshot unavailable locally",
                caveats,
            ));
        }
        Err(err) => return Err(err),
    };
    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);
    let target_risk =
        target_risk_from_manifest_conflicts(repo, &base_manifest, &outcome.conflicts)?;
    let conflicts =
        predict_conflicts(repo, &base_manifest, outcome.conflicts, branch, parent_name)?;
    let merged_manifest = Manifest::new(outcome.clean_entries);
    let changes = diff_manifests_with_content_renames(repo, &parent_manifest, &merged_manifest)?;
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
    triage_action: RecommendedAction,
    triage_command: &str,
) -> Vec<String> {
    let remote_flag = if remote { " --remote" } else { "" };
    if triage_action == RecommendedAction::Close {
        return vec![triage_command.to_string()];
    }
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

fn merge_base_unavailable_caveat(
    parent_name: &str,
    reason: &crate::commands::merge::MergeBaseUnavailable,
) -> String {
    match reason {
        crate::commands::merge::MergeBaseUnavailable::IncompleteAncestry { .. } => format!(
            "Parent '{parent_name}' ancestry is incomplete locally; remote conflict prediction is unavailable."
        ),
        crate::commands::merge::MergeBaseUnavailable::IncompleteManifestData { .. } => format!(
            "Parent '{parent_name}' manifest data is incomplete locally; remote conflict prediction is unavailable."
        ),
        crate::commands::merge::MergeBaseUnavailable::IncompleteBlobData { .. } => format!(
            "Parent '{parent_name}' blob data is incomplete locally; remote conflict prediction is unavailable."
        ),
        crate::commands::merge::MergeBaseUnavailable::NoVerifiedCommonAncestor => format!(
            "No verified common ancestor with parent '{parent_name}' was found locally; remote conflict prediction is unavailable."
        ),
    }
}

fn merge_base_unavailable_reason(
    reason: &crate::commands::merge::MergeBaseUnavailable,
) -> &'static str {
    match reason {
        crate::commands::merge::MergeBaseUnavailable::IncompleteAncestry { .. } => {
            "parent ancestry incomplete locally"
        }
        crate::commands::merge::MergeBaseUnavailable::IncompleteManifestData { .. } => {
            "parent manifest data incomplete locally"
        }
        crate::commands::merge::MergeBaseUnavailable::IncompleteBlobData { .. } => {
            "parent blob data incomplete locally"
        }
        crate::commands::merge::MergeBaseUnavailable::NoVerifiedCommonAncestor => {
            "no verified common ancestor locally"
        }
    }
}

struct BranchDiffComputed {
    changed_files: Vec<FileSummaryJson>,
    changed_file_count: usize,
    identical: Option<IdenticalFilesJson>,
    lineage_comparison_head: Option<Hash>,
    lineage_comparison_source: String,
    extra_caveats: Vec<String>,
    /// Net-merge only: paths predicted to conflict (excluded from the
    /// clean-path diff, surfaced so they never silently disappear).
    conflict_paths: Vec<String>,
}

struct PredictedMergeAgainst {
    available: bool,
    against_manifest: Option<Manifest>,
    merged_manifest: Manifest,
    /// Paths the three-way prediction says would conflict, sorted. The count
    /// is `conflict_paths.len()`; the paths themselves let diff surfaces mark
    /// the files inline instead of only reporting a number.
    conflict_paths: Vec<String>,
    caveats: Vec<String>,
}

struct TreeDiffEvidence {
    changes: Vec<FileChange>,
    changed_files: Vec<FileSummaryJson>,
    changed_file_count: usize,
    identical: IdenticalFilesJson,
    extra_caveats: Vec<String>,
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
            let evidence = tree_diff_evidence(repo, comparison, branch, against)?;
            let unavailable =
                !comparison.branch_snapshot_available || !comparison.comparison_snapshot_available;
            Ok(BranchDiffComputed {
                changed_file_count: evidence.changed_file_count,
                changed_files: evidence.changed_files,
                identical: Some(evidence.identical),
                lineage_comparison_head: comparison.comparison_head.clone(),
                lineage_comparison_source: if unavailable {
                    "diff_unavailable".to_string()
                } else {
                    comparison.comparison_source.clone()
                },
                extra_caveats: evidence.extra_caveats,
                conflict_paths: Vec::new(),
            })
        }
        DiffMode::Contribution => {
            let mut extra_caveats = Vec::new();
            if !comparison.branch_snapshot_available {
                extra_caveats.push(format!(
                    "Branch '{branch}' snapshot is incomplete locally; contribution diff is unavailable."
                ));
                return Ok(unavailable_branch_diff(
                    comparison.fork_point.clone(),
                    against,
                    "contribution_unavailable",
                    extra_caveats,
                ));
            }
            let (fork_manifest, fork_head, source) = if let Some(fork) =
                comparison.fork_point.as_ref()
            {
                match manifest_for_head(repo, Some(fork)) {
                    Ok(manifest) => (manifest, Some(fork.clone()), "fork_point".to_string()),
                    Err(err) if is_local_merge_data_unavailable(&err) => {
                        extra_caveats.push(
                            "Fork point snapshot is incomplete locally; contribution diff is unavailable."
                                .to_string(),
                        );
                        return Ok(unavailable_branch_diff(
                            Some(fork.clone()),
                            against,
                            "contribution_unavailable",
                            extra_caveats,
                        ));
                    }
                    Err(err) => return Err(err),
                }
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
            let changes = diff_manifests_with_content_renames(
                repo,
                &fork_manifest,
                &comparison.branch_manifest,
            )?;
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
                conflict_paths: Vec::new(),
            })
        }
        DiffMode::NetMerge => {
            let mut extra_caveats = Vec::new();
            if !comparison.branch_snapshot_available || !comparison.comparison_snapshot_available {
                extra_caveats.push("Branch or against snapshot is incomplete locally; net-merge prediction is unavailable.".to_string());
                return Ok(BranchDiffComputed {
                    changed_file_count: 0,
                    changed_files: Vec::new(),
                    identical: None,
                    lineage_comparison_head: comparison.against_head.clone(),
                    lineage_comparison_source: "net_merge_unavailable".to_string(),
                    extra_caveats,
                    conflict_paths: Vec::new(),
                });
            }
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
                    conflict_paths: Vec::new(),
                });
            };
            let prediction = predict_merge_against(
                repo,
                branch,
                against,
                &comparison.branch_manifest,
                against_head,
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
                    conflict_paths: Vec::new(),
                });
            }
            let against_manifest = prediction
                .against_manifest
                .as_ref()
                .expect("available net-merge prediction includes against manifest");
            let changes = drop_predicted_conflicts(
                diff_manifests_with_content_renames(
                    repo,
                    against_manifest,
                    &prediction.merged_manifest,
                )?,
                &prediction.conflict_paths,
            );
            let changed_files = summarize_manifest_changes(repo, &changes)?;
            if !prediction.conflict_paths.is_empty() {
                extra_caveats.push(format!(
                    "Merge prediction found {} conflict file(s); net-merge diff includes only clean merge paths.",
                    prediction.conflict_paths.len()
                ));
            }
            Ok(BranchDiffComputed {
                changed_file_count: changed_files.len(),
                changed_files,
                identical: Some(identical_files(
                    &prediction.merged_manifest,
                    against_manifest,
                    against,
                )),
                lineage_comparison_head: comparison.against_head.clone(),
                lineage_comparison_source: "predicted_merge_result".to_string(),
                extra_caveats,
                conflict_paths: prediction.conflict_paths,
            })
        }
    }
}

/// The manifest pair one branch-mode diff renders from, plus everything the
/// caller needs to present it honestly: caveats about incomplete local data
/// and (in net-merge mode) the paths the merge prediction says would
/// conflict.
///
/// This is the checkout-free evidence layer `oak diff <branch>` shares with
/// `oak branch diff --json`: both resolve endpoints through here, so the
/// human hunks and the machine summaries can never disagree about *what*
/// changed — only about how it is presented.
pub(crate) struct EndpointManifests {
    /// Pre-image side (`a/` in the rendered diff).
    pub(crate) base: Manifest,
    /// Post-image side (`b/` in the rendered diff).
    pub(crate) target: Manifest,
    /// Why parts of the evidence may be missing or approximate.
    pub(crate) caveats: Vec<String>,
    /// Net-merge only: paths predicted to conflict (empty otherwise).
    pub(crate) conflict_paths: Vec<String>,
    /// Where the base side came from, for lineage/labels (e.g.
    /// "fork_point", "against_head", "predicted_merge_result").
    #[allow(dead_code)]
    pub(crate) base_source: String,
}

impl EndpointManifests {
    /// The change set between base and target — with full content-rename
    /// detection, so endpoint diffs report the same R entries as `oak
    /// branch diff --json` — and with predicted-conflict paths removed. A
    /// conflicted path is absent from the predicted merge manifest, so a
    /// naive manifest diff would misrepresent it as a deletion; conflicts
    /// are named in `conflict_paths` instead, never disguised as clean
    /// changes.
    pub(crate) fn changes(&self, repo: &dyn Repository) -> Result<Vec<FileChange>> {
        Ok(drop_predicted_conflicts(
            diff_manifests_with_content_renames(repo, &self.base, &self.target)?,
            &self.conflict_paths,
        ))
    }
}

/// Remove rows for paths the merge prediction says would conflict — they
/// are reported as conflicts, not as clean (usually "deleted") changes.
pub(crate) fn drop_predicted_conflicts(
    mut changes: Vec<FileChange>,
    conflict_paths: &[String],
) -> Vec<FileChange> {
    if conflict_paths.is_empty() {
        return changes;
    }
    let conflicted: HashSet<&str> = conflict_paths.iter().map(String::as_str).collect();
    changes.retain(|change| !conflicted.contains(change.path.as_str()));
    changes
}

/// Resolve the (base, target) manifests for diffing `branch` against
/// `against` under `diff_mode`. Shared by the human-facing endpoint diff
/// (`oak diff <branch>`) and kept in lock-step with [`compute_branch_diff`],
/// which produces the JSON summaries over the same manifest pairs.
pub(crate) fn branch_mode_manifests(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
) -> Result<EndpointManifests> {
    let comparison = branch_comparison(repo, branch, against)?;
    let mut caveats = comparison.caveats.clone();
    if !comparison.branch_snapshot_available {
        return Err(OakError::IncompleteManifestData {
            left: branch.to_string(),
            right: against.to_string(),
            missing: format!("branch '{branch}' snapshot (incomplete locally)"),
        });
    }
    match diff_mode {
        DiffMode::Tree => {
            if !comparison.comparison_snapshot_available {
                return Err(OakError::IncompleteManifestData {
                    left: branch.to_string(),
                    right: against.to_string(),
                    missing: format!("branch '{against}' snapshot (incomplete locally)"),
                });
            }
            Ok(EndpointManifests {
                base: comparison.comparison_manifest,
                target: comparison.branch_manifest,
                caveats,
                conflict_paths: Vec::new(),
                base_source: comparison.comparison_source,
            })
        }
        DiffMode::Contribution => {
            let (base, source) = match comparison.fork_point.as_ref() {
                Some(fork) => (manifest_for_head(repo, Some(fork))?, "fork_point"),
                None => {
                    caveats.push(format!(
                        "Fork point between branch '{branch}' and '{against}' was unavailable; compared against the empty tree."
                    ));
                    (Manifest::empty(), "fork_point_unavailable_empty_tree")
                }
            };
            Ok(EndpointManifests {
                base,
                target: comparison.branch_manifest,
                caveats,
                conflict_paths: Vec::new(),
                base_source: source.to_string(),
            })
        }
        DiffMode::NetMerge => {
            let against_head = comparison.against_head.as_ref().ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "against branch '{against}' has no local head; net-merge prediction is unavailable"
                ))
            })?;
            let prediction = predict_merge_against(
                repo,
                branch,
                against,
                &comparison.branch_manifest,
                against_head,
            )?;
            caveats.extend(prediction.caveats.clone());
            if !prediction.available {
                return Err(OakError::InvalidArgument(format!(
                    "net-merge prediction between '{branch}' and '{against}' is unavailable locally; {}",
                    prediction.caveats.join("; ")
                )));
            }
            let against_manifest = prediction
                .against_manifest
                .clone()
                .expect("available net-merge prediction includes against manifest");
            if !prediction.conflict_paths.is_empty() {
                caveats.push(format!(
                    "Merge prediction found {} conflict file(s); net-merge diff includes only clean merge paths.",
                    prediction.conflict_paths.len()
                ));
            }
            Ok(EndpointManifests {
                base: against_manifest,
                target: prediction.merged_manifest,
                caveats,
                conflict_paths: prediction.conflict_paths,
                base_source: "predicted_merge_result".to_string(),
            })
        }
    }
}

fn tree_diff_evidence(
    repo: &dyn Repository,
    comparison: &BranchComparison,
    branch: &str,
    against: &str,
) -> Result<TreeDiffEvidence> {
    let mut caveats = Vec::new();
    if !comparison.branch_snapshot_available {
        caveats.push(format!(
            "Branch '{branch}' snapshot is incomplete locally; tree diff is unavailable."
        ));
    }
    if !comparison.comparison_snapshot_available {
        caveats.push(format!(
            "Comparison snapshot for branch '{against}' is incomplete locally; tree diff is unavailable."
        ));
    }
    if !caveats.is_empty() {
        return Ok(TreeDiffEvidence {
            changes: Vec::new(),
            changed_files: Vec::new(),
            changed_file_count: 0,
            identical: identical_files_unavailable(against, caveats.clone()),
            extra_caveats: caveats,
        });
    }

    let changes = diff_manifests_with_content_renames(
        repo,
        &comparison.comparison_manifest,
        &comparison.branch_manifest,
    )?;
    let changed_files = summarize_manifest_changes(repo, &changes)?;
    Ok(TreeDiffEvidence {
        changed_file_count: changed_files.len(),
        changed_files,
        identical: identical_files(
            &comparison.branch_manifest,
            &comparison.comparison_manifest,
            against,
        ),
        changes,
        extra_caveats: Vec::new(),
    })
}

fn unavailable_branch_diff(
    comparison_head: Option<Hash>,
    against: &str,
    source: &str,
    extra_caveats: Vec<String>,
) -> BranchDiffComputed {
    BranchDiffComputed {
        changed_file_count: 0,
        changed_files: Vec::new(),
        identical: Some(identical_files_unavailable(against, extra_caveats.clone())),
        lineage_comparison_head: comparison_head,
        lineage_comparison_source: source.to_string(),
        extra_caveats,
        conflict_paths: Vec::new(),
    }
}

fn predict_merge_against(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
    branch_manifest: &Manifest,
    against_head: &Hash,
) -> Result<PredictedMergeAgainst> {
    let resolution =
        crate::commands::merge::resolve_merge_base(repo, branch, against, Some(against_head))?;
    let base_manifest = match resolution {
        crate::commands::merge::MergeBaseResolution::CommonAncestor(base_manifest)
        | crate::commands::merge::MergeBaseResolution::DeclaredRoot(base_manifest) => base_manifest,
        crate::commands::merge::MergeBaseResolution::Unavailable(reason) => {
            return Ok(PredictedMergeAgainst {
                available: false,
                against_manifest: None,
                merged_manifest: Manifest::empty(),
                conflict_paths: Vec::new(),
                caveats: vec![merge_base_unavailable_caveat(against, &reason)],
            });
        }
    };
    let against_manifest = match manifest_for_head(repo, Some(against_head)) {
        Ok(manifest) => manifest,
        Err(err) if is_local_merge_data_unavailable(&err) => {
            return Ok(PredictedMergeAgainst {
                available: false,
                against_manifest: None,
                merged_manifest: Manifest::empty(),
                conflict_paths: Vec::new(),
                caveats: vec![format!(
                    "Against branch '{against}' snapshot is incomplete locally; net-merge prediction is unavailable."
                )],
            });
        }
        Err(err) => return Err(err),
    };
    let outcome = three_way_merge_manifests(&base_manifest, branch_manifest, &against_manifest);
    let conflicts = predict_conflicts(repo, &base_manifest, outcome.conflicts, branch, against)?;
    let mut conflict_paths: Vec<String> = conflicts.iter().map(|c| c.path.clone()).collect();
    conflict_paths.sort();
    Ok(PredictedMergeAgainst {
        available: true,
        against_manifest: Some(against_manifest),
        merged_manifest: Manifest::new(outcome.clean_entries),
        conflict_paths,
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

/// Is `name` a branch endpoint even though no local branch row exists?
///
/// The default branch normally has no row after a fresh `oak init` — Oak
/// treats `main` as server-owned — and any name recorded as another branch's
/// `parent_branch` denotes a real branch whether or not it is local. Both
/// must resolve as branch endpoints instead of failing with a false
/// "not a branch" error; downstream comparison code carries the honest
/// fallbacks and caveats for their possibly-missing heads.
pub(crate) fn is_rowless_branch_name(repo: &dyn Repository, name: &str) -> Result<bool> {
    if name == DEFAULT_BRANCH {
        return Ok(true);
    }
    Ok(repo
        .list_branches()?
        .iter()
        .any(|branch| branch.parent_branch.as_deref() == Some(name)))
}

pub(crate) fn branch_comparison(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
) -> Result<BranchComparison> {
    let branch_row = match repo.get_branch(branch)? {
        Some(row) => Some(row),
        // A rowless-but-real branch (the server-owned default branch, or a
        // parent recorded on another branch) is compared from whatever local
        // evidence exists, with caveats — not rejected as nonexistent.
        None if is_rowless_branch_name(repo, branch)? => None,
        None => return Err(OakError::BranchNotFound(branch.to_string())),
    };
    let branch_own_head = repo.get_branch_head(branch)?;
    let branch_head = crate::commands::commit::resolve_effective_head(repo, branch)?;
    let mut caveats = Vec::new();
    if branch_row.is_none() && branch_head.is_none() {
        caveats.push(format!(
            "Branch '{branch}' has no local head; its side of the comparison is the empty tree."
        ));
        if branch == DEFAULT_BRANCH {
            caveats.push("Local main may be absent because Oak treats main as server-owned; this command does not fetch it.".to_string());
        }
    }
    let (branch_manifest, branch_snapshot_available) = match manifest_for_head(
        repo,
        branch_head.as_ref(),
    ) {
        Ok(manifest) => (manifest, true),
        Err(err) if is_local_merge_data_unavailable(&err) => {
            caveats.push(format!(
                "Branch '{branch}' snapshot is incomplete locally; read-only merge prediction may be unavailable."
            ));
            (Manifest::empty(), false)
        }
        Err(err) => return Err(err),
    };
    let against_head = repo.get_branch_head(against)?;
    let commits = repo.get_commits_for_branch(branch)?;
    let fork_head = commits.first().and_then(|c| c.parent_hash.clone());
    let (comparison_head, comparison_source) = if against_head.is_some() {
        (against_head.clone(), format!("local_branch_head:{against}"))
    } else if let Some(fork) = fork_head.clone() {
        caveats.push(format!(
            "Against branch '{against}' has no local head; compared against the first branch commit's parent instead."
        ));
        (
            Some(fork),
            "branch_first_commit_parent_fallback".to_string(),
        )
    } else {
        caveats.push(format!(
            "Against branch '{against}' has no local head and no branch fork point was found; compared against the empty tree."
        ));
        (None, "empty_tree_fallback".to_string())
    };
    if against == DEFAULT_BRANCH && against_head.is_none() {
        caveats.push("Local main may be absent because Oak treats main as server-owned; this command does not fetch it.".to_string());
    }
    let (comparison_manifest, comparison_snapshot_available) = match manifest_for_head(
        repo,
        comparison_head.as_ref(),
    ) {
        Ok(manifest) => (manifest, true),
        Err(err) if is_local_merge_data_unavailable(&err) => {
            caveats.push(format!(
                    "Comparison head for branch '{against}' is incomplete locally; read-only merge prediction may be unavailable."
                ));
            (Manifest::empty(), false)
        }
        Err(err) => return Err(err),
    };
    let fork_point = match (branch_head.as_ref(), comparison_head.as_ref()) {
        (Some(branch_head), Some(comparison_head)) => {
            match first_common_ancestor(repo, branch_head, comparison_head)? {
                ForkPointResolution::Found(hash) => Some(hash),
                ForkPointResolution::Unavailable { missing } => {
                    caveats.push(format!(
                    "Local lineage is incomplete; fork point is unavailable because commit(s) {} are missing. Run 'oak fetch' or 'oak pull --force' to repair local history.",
                    format_missing_hashes(&missing)
                ));
                    None
                }
                ForkPointResolution::None => None,
            }
        }
        _ => None,
    };

    Ok(BranchComparison {
        parent: branch_row.and_then(|row| row.parent_branch),
        branch_own_head,
        branch_head,
        against_head,
        comparison_head,
        comparison_source,
        fork_point,
        branch_commit_count: commits.len(),
        branch_manifest,
        branch_snapshot_available,
        comparison_manifest,
        comparison_snapshot_available,
        caveats,
    })
}

/// The manifest a branch's whole contribution should be diffed against: the
/// fork point between `branch` and `against` (the same base `oak branch diff
/// --diff-mode contribution` uses), so the diff shows what the branch changed
/// without picking up unrelated movement on `against`. Falls back to the empty
/// tree — with a caveat — when no fork point can be found.
pub(crate) fn branch_fork_base_manifest(
    repo: &dyn Repository,
    branch: &str,
    against: &str,
) -> Result<(Manifest, Vec<String>)> {
    let comparison = branch_comparison(repo, branch, against)?;
    match comparison.fork_point.as_ref() {
        Some(fork) => {
            // A branch that never merged `against` in forks exactly at its
            // first commit's parent, so an absent local `against` head loses
            // nothing — drop branch_comparison's caveats about it rather than
            // warning on every offline diff. Once a merge commit pulls
            // `against` in, the true fork point depends on its head, so the
            // caveats stay.
            let pulled_against_in = repo
                .get_commits_for_branch(branch)?
                .iter()
                .any(|c| c.merge_parent_hash.is_some());
            let caveats = if pulled_against_in {
                comparison.caveats.clone()
            } else {
                Vec::new()
            };
            Ok((manifest_for_head(repo, Some(fork))?, caveats))
        }
        None => {
            let mut caveats = comparison.caveats.clone();
            caveats.push(format!(
                "Fork point between branch '{branch}' and '{against}' was unavailable; compared against the empty tree."
            ));
            Ok((Manifest::empty(), caveats))
        }
    }
}

pub(crate) fn manifest_for_head(repo: &dyn Repository, head: Option<&Hash>) -> Result<Manifest> {
    let Some(head) = head else {
        return Ok(Manifest::empty());
    };
    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    repo.get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))
}

fn is_local_merge_data_unavailable(err: &OakError) -> bool {
    matches!(
        err,
        OakError::CommitNotFound(_)
            | OakError::ManifestNotFound(_)
            | OakError::IncompleteCommitData { .. }
            | OakError::IncompleteManifestData { .. }
            | OakError::IncompleteBlobData { .. }
    )
}

/// Manifest-to-manifest diff with full rename detection — exact-hash renames
/// plus content-similarity promotion of rename-with-edit pairs — so the
/// checkout-free branch diffs report the same R entries `oak log` records and
/// `oak diff` shows. Uses the default [`RenameConfig`] caps; blobs missing
/// locally or above the size cap simply stay Delete+Add.
pub(crate) fn diff_manifests_with_content_renames(
    repo: &dyn Repository,
    old: &Manifest,
    new: &Manifest,
) -> Result<Vec<FileChange>> {
    old.diff_with_content_renames(
        new,
        |h| Ok(repo.get_blob(h)?.map(|b| b.content)),
        &RenameConfig::default(),
    )
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
            // The content is not locally readable (not hydrated), so only
            // path-derived categories apply; claiming "large" here would be
            // a false signal about content we never saw. Absent = unknown.
            category: classify_change(&change.path, None, None, false),
            patch: None,
            patch_omitted: false,
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
        category: classify_change(&change.path, old_bytes, new_bytes, binary_or_large),
        patch: None,
        patch_omitted: false,
    }
}

/// Basenames that are dependency lockfiles: high-churn, machine-written,
/// rarely worth reading line by line.
const LOCKFILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Gemfile.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
    "packages.lock.json",
];

/// Path components that mark vendored/third-party trees.
const VENDORED_COMPONENTS: &[&str] = &["vendor", "node_modules", "third_party", "third-party"];

/// Classify a changed file so consumers can skip noise without reading it:
/// `lockfile`, `vendored`, `generated`, `binary`, `large` — or `None` for
/// ordinary source (omitted from JSON; absent means source).
///
/// Heuristics only, and deliberately cheap: basename and path-component
/// matches, plus a "@generated" / "DO NOT EDIT" marker scan over the first
/// few lines (bounded to 1 KiB) of whichever side is present — headers only,
/// per the git convention, so source that merely *mentions* a marker deeper
/// in the file is not misfiled. Wrong guesses are recoverable — the category
/// never hides a file, it only annotates it.
pub(crate) fn classify_change(
    path: &str,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
    binary_or_large: bool,
) -> Option<&'static str> {
    let basename = path.rsplit('/').next().unwrap_or(path);
    if LOCKFILE_NAMES.contains(&basename) {
        return Some("lockfile");
    }
    if path
        .split('/')
        .any(|component| VENDORED_COMPONENTS.contains(&component))
    {
        return Some("vendored");
    }
    if basename.ends_with(".min.js") || basename.ends_with(".min.css") || basename.ends_with(".map")
    {
        return Some("generated");
    }
    // Generated-file markers count only in the header — the first few
    // lines — like git's `@generated` convention. A body line that merely
    // mentions the marker (e.g. code that *prints* a DO NOT EDIT banner)
    // must not reclassify real source.
    const GENERATED_MARKER_HEADER_LINES: usize = 5;
    let head_has_marker = |bytes: Option<&[u8]>| {
        bytes.is_some_and(|bytes| {
            let head = &bytes[..bytes.len().min(1024)];
            let head = String::from_utf8_lossy(head);
            head.lines()
                .take(GENERATED_MARKER_HEADER_LINES)
                .any(|line| line.contains("@generated") || line.contains("DO NOT EDIT"))
        })
    };
    if head_has_marker(new_bytes) || head_has_marker(old_bytes) {
        return Some("generated");
    }
    if binary_or_large {
        let is_binary = |bytes: Option<&[u8]>| {
            bytes.is_some_and(|bytes| bytes[..bytes.len().min(8192)].contains(&0))
        };
        return Some(if is_binary(new_bytes) || is_binary(old_bytes) {
            "binary"
        } else {
            "large"
        });
    }
    None
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

fn identical_files_unavailable(against: &str, caveats: Vec<String>) -> IdenticalFilesJson {
    IdenticalFilesJson {
        available: false,
        against: against.to_string(),
        count: 0,
        files_sample: Vec::new(),
        caveats,
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

enum ForkPointResolution {
    Found(Hash),
    Unavailable { missing: Vec<Hash> },
    None,
}

fn first_common_ancestor(
    repo: &dyn Repository,
    left: &Hash,
    right: &Hash,
) -> Result<ForkPointResolution> {
    let left_walk = collect_ancestors(repo, left)?;
    let right_walk = collect_ancestors(repo, right)?;
    let right_ancestors: HashSet<String> = right_walk
        .ancestors
        .iter()
        .map(|hash| hash.to_string())
        .collect();
    for hash in &left_walk.ancestors {
        if right_ancestors.contains(&hash.to_string()) {
            let left_depth = left_walk.depth(hash).unwrap_or(usize::MAX);
            let right_depth = right_walk.depth(hash).unwrap_or(usize::MAX);
            let missing =
                missing_edges_that_can_hide_fork(&left_walk, left_depth, &right_walk, right_depth);
            if !missing.is_empty() {
                return Ok(ForkPointResolution::Unavailable { missing });
            }
            return Ok(ForkPointResolution::Found(hash.clone()));
        }
    }
    let missing = all_missing_edges(&left_walk, &right_walk);
    if !missing.is_empty() {
        return Ok(ForkPointResolution::Unavailable { missing });
    }
    Ok(ForkPointResolution::None)
}

#[derive(Default)]
struct ReviewAncestorWalk {
    ancestors: Vec<Hash>,
    depths: HashMap<String, usize>,
    missing_edges: Vec<ReviewMissingEdge>,
}

impl ReviewAncestorWalk {
    fn depth(&self, hash: &Hash) -> Option<usize> {
        self.depths.get(hash.as_str()).copied()
    }
}

struct ReviewMissingEdge {
    hash: Hash,
    depth: usize,
}

fn collect_ancestors(repo: &dyn Repository, head: &Hash) -> Result<ReviewAncestorWalk> {
    let mut out = Vec::new();
    let mut depths = HashMap::new();
    let mut missing_edges = Vec::new();
    let mut queue = VecDeque::from([(head.clone(), 0usize)]);
    let mut seen = HashSet::new();
    while let Some((hash, depth)) = queue.pop_front() {
        if !seen.insert(hash.to_string()) {
            continue;
        }
        let Some(commit) = repo.get_commit(&hash)? else {
            missing_edges.push(ReviewMissingEdge { hash, depth });
            continue;
        };
        depths.insert(hash.to_string(), depth);
        out.push(hash);
        if let Some(parent) = commit.parent_hash {
            queue.push_back((parent, depth + 1));
        }
        if let Some(merge_parent) = commit.merge_parent_hash {
            queue.push_back((merge_parent, depth + 1));
        }
    }
    Ok(ReviewAncestorWalk {
        ancestors: out,
        depths,
        missing_edges,
    })
}

fn missing_edges_that_can_hide_fork(
    left: &ReviewAncestorWalk,
    left_depth: usize,
    right: &ReviewAncestorWalk,
    right_depth: usize,
) -> Vec<Hash> {
    left.missing_edges
        .iter()
        .filter(|edge| edge.depth <= left_depth)
        .chain(
            right
                .missing_edges
                .iter()
                .filter(|edge| edge.depth <= right_depth),
        )
        .map(|edge| edge.hash.clone())
        .collect()
}

fn all_missing_edges(left: &ReviewAncestorWalk, right: &ReviewAncestorWalk) -> Vec<Hash> {
    left.missing_edges
        .iter()
        .chain(right.missing_edges.iter())
        .map(|edge| edge.hash.clone())
        .collect()
}

fn format_missing_hashes(hashes: &[Hash]) -> String {
    let mut missing: Vec<String> = hashes.iter().map(|h| h.short().to_string()).collect();
    missing.sort();
    missing.dedup();
    missing.join(", ")
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
    let tree_evidence = tree_diff_evidence(repo, &comparison, branch, against)?;
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
        &tree_evidence.changes,
        &tree_evidence.changed_files,
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
            local_snapshot_missing: !comparison.branch_snapshot_available
                || !comparison.comparison_snapshot_available,
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
    files_identical_to_against: IdenticalFilesJson,
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
        files_identical_to_against,
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
