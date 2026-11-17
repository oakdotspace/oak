use super::*;

#[test]
fn next_commands_shell_quote_branch_names() {
    let hostile = "x;touch${IFS}pwned";
    let quoted = "'x;touch${IFS}pwned'";
    let commands = merge_next_commands(hostile, false, false, MergeReadiness::Conflicts);
    assert_eq!(
        commands,
        vec![
            format!("oak switch {quoted}"),
            "oak pull".to_string(),
            format!("oak merge {quoted}"),
        ]
    );
    for command in branch_diff_recommendations(hostile, "main", false, DiffMode::Tree)
        .into_iter()
        .chain([review_page_command(hostile, true, true, 10, 20)])
        .chain([diff_page_command(
            hostile,
            "m$(id)",
            false,
            DiffMode::Tree,
            1,
            2,
        )])
    {
        assert!(command.contains(quoted), "{command}");
        assert!(!command.contains(&format!(" {hostile}")), "{command}");
    }
    assert!(
        diff_page_command("b", "m$(id)", false, DiffMode::Tree, 1, 2)
            .contains("--against 'm$(id)'")
    );
    // Plain names are unchanged.
    assert_eq!(
        merge_next_commands("feature-1", true, false, MergeReadiness::Clean),
        vec!["oak merge feature-1".to_string()]
    );
}

// ---- fork point / merge base certification on partial graphs -----------

use crate::commands::merge::merge_base_fixtures as fx;
use crate::commands::merge::{
    resolve_merge_base_identity_from_heads, MergeBaseIdentity, MergeBaseUnavailable,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Base(Hash),
    Unavailable(Vec<Hash>),
    Other(String),
}

fn fork_point(repo: &dyn Repository, a: &Hash, b: &Hash) -> Resolved {
    match first_common_ancestor(repo, a, b).unwrap() {
        ForkPointResolution::Found(hash) => Resolved::Base(hash),
        ForkPointResolution::Unavailable { missing } => Resolved::Unavailable(missing),
        ForkPointResolution::None => Resolved::Other("none".into()),
    }
}

fn merge_base(repo: &dyn Repository, a: &Hash, b: &Hash) -> Resolved {
    match resolve_merge_base_identity_from_heads(repo, "a", Some(a), None, "b", Some(b)).unwrap() {
        MergeBaseIdentity::Commit(hash) => Resolved::Base(hash),
        MergeBaseIdentity::Unavailable(MergeBaseUnavailable::IncompleteAncestry {
            missing,
            ..
        }) => Resolved::Unavailable(missing),
        MergeBaseIdentity::Unavailable(other) => Resolved::Other(format!("{other:?}")),
        MergeBaseIdentity::DeclaredRoot => Resolved::Other("declared-root".into()),
    }
}

#[test]
fn fork_point_partial_graph_never_certifies_stale_base_behind_target_gap() {
    let s = fx::squash_revert();
    let (_full_dir, full) = s.graph.materialize(|_| true);
    assert_eq!(fork_point(&full, &s.a1, &s.h), Resolved::Base(s.x.clone()));
    for dropped in [
        vec![s.m[6].clone()],
        std::iter::once(s.r.clone())
            .chain(s.m[..7].iter().cloned())
            .collect::<Vec<_>>(),
    ] {
        let (_dir, partial) = s.graph.materialize(|c| !dropped.contains(&c.hash));
        match fork_point(&partial, &s.a1, &s.h) {
            Resolved::Base(base) => {
                assert_ne!(base, s.c, "review fork point certified stale base C");
                assert_eq!(base, s.x);
            }
            Resolved::Unavailable(missing) => {
                assert!(dropped.iter().any(|d| missing.contains(d)), "{missing:?}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Differential property: on seeded random Oak-shaped DAGs with random
/// shallow boundaries and holes, the bounded (partial-graph) result of
/// both resolvers is the complete-graph result or unavailable; the
/// complete-graph result is a best common ancestor (independent oracle);
/// and replaying the review hydration loop (fetch the reported gaps,
/// retry) converges on the complete-graph result.
#[test]
fn partial_graph_merge_base_is_full_graph_result_or_unavailable() {
    // 64 seeds keep CI fast; set OAK_MERGE_BASE_PROPERTY_SEEDS for a soak.
    let seeds: u64 = std::env::var("OAK_MERGE_BASE_PROPERTY_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    let mut violations: Vec<String> = Vec::new();
    let mut stats = [0usize; 4]; // [pairs, bounded-certified, bounded-unavailable, hydration rounds]
    for seed in 0..seeds {
        let mut rng = fx::Rng::new(0x0a4b_5eed ^ seed);
        let dag = fx::random_oak_dag(&mut rng);
        let (_full_dir, full) = dag.graph.materialize(|_| true);
        let main_head = dag.main.last().unwrap().clone();
        let mut pairs: Vec<(Hash, Hash)> = dag
            .branch_heads
            .iter()
            .map(|head| (head.clone(), main_head.clone()))
            .collect();
        if dag.branch_heads.len() >= 2 {
            let i = rng.below(dag.branch_heads.len());
            let j = rng.below(dag.branch_heads.len());
            pairs.push((dag.branch_heads[i].clone(), dag.branch_heads[j].clone()));
        }
        if let Some(head) = dag.branch_heads.first() {
            pairs.push((main_head.clone(), head.clone()));
        }
        let n = dag.graph.commits.len();
        for (a, b) in pairs {
            stats[0] += 1;
            let best = dag.graph.best_common_ancestors(&a, &b);
            let full_mb = merge_base(&full, &a, &b);
            let full_fp = fork_point(&full, &a, &b);
            if full_mb != full_fp {
                violations.push(format!(
                    "seed {seed}: full merge base {full_mb:?} != fork point {full_fp:?}"
                ));
            }
            let Resolved::Base(truth) = full_mb.clone() else {
                violations.push(format!(
                    "seed {seed}: full graph did not resolve: {full_mb:?}"
                ));
                continue;
            };
            if !best.contains(&truth) {
                violations.push(format!(
                    "seed {seed}: full-graph base {} is not a best common ancestor",
                    truth.short()
                ));
            }
            // Partial shapes: shallow boundary, random holes, both.
            for variant in 0..3 {
                let cutoff = if variant != 1 { rng.below(n) } else { 0 };
                let hole_pct = if variant != 0 {
                    10 + rng.below(30) as u64
                } else {
                    0
                };
                let dropped: std::collections::HashSet<Hash> = dag
                    .graph
                    .commits
                    .iter()
                    .enumerate()
                    .filter(|(i, c)| {
                        c.hash != a && c.hash != b && (*i < cutoff || rng.chance(hole_pct))
                    })
                    .map(|(_, c)| c.hash.clone())
                    .collect();
                let (_dir, partial) = dag.graph.materialize(|c| !dropped.contains(&c.hash));
                for (name, bounded) in [
                    ("merge base", merge_base(&partial, &a, &b)),
                    ("fork point", fork_point(&partial, &a, &b)),
                ] {
                    match &bounded {
                            Resolved::Base(base) if *base == truth => stats[1] += 1,
                            Resolved::Unavailable(_) => stats[2] += 1,
                            other => violations.push(format!(
                                "seed {seed} variant {variant} {name}: bounded {other:?} but full graph {} ({} of {n} rows dropped)",
                                truth.short(),
                                dropped.len()
                            )),
                        }
                }
                // Hydration loop convergence (review preparation policy).
                let rounds = match fx::hydrate_like_review(&dag.graph, &partial, &a, &b, n + 2) {
                    Ok((MergeBaseIdentity::Commit(base), rounds)) => {
                        if base != truth {
                            violations.push(format!(
                                "seed {seed} variant {variant}: hydration certified {} not {}",
                                base.short(),
                                truth.short()
                            ));
                        }
                        rounds
                    }
                    Ok((_, rounds)) => {
                        violations.push(format!(
                            "seed {seed} variant {variant}: hydration did not resolve a commit"
                        ));
                        rounds
                    }
                    Err(error) => {
                        violations.push(format!("seed {seed} variant {variant}: {error}"));
                        0
                    }
                };
                stats[3] += rounds;
            }
        }
    }
    eprintln!(
        "pairs={} bounded_certified={} bounded_unavailable={} hydration_rounds={} violations={}",
        stats[0],
        stats[1],
        stats[2],
        stats[3],
        violations.len()
    );
    assert!(
        violations.is_empty(),
        "{} violations; first 10:\n{}",
        violations.len(),
        violations
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The property must exercise both outcomes to be meaningful.
    assert!(
        stats[1] > 0 && stats[2] > 0,
        "degenerate coverage: {stats:?}"
    );
}

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
        evidence_kind: EVIDENCE_PREDICTED_MERGE,
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
        invariant_violations: Some(Vec::new()),
        // A certified block: this helper models an ordinary healthy
        // preview, and a claimed prediction without a block now fails
        // closed as uncertified.
        merge_safety: Some(crate::commands::merge_safety::MergeSafetyJson {
            assessment_kind: "four_tree",
            certified: true,
            verdict: "safe",
            uncertified_cause: None,
            target_head_source: "live_remote_fetch",
            assessed_target: "main".to_string(),
            fetched_target: Some("main".to_string()),
            fetched_target_head: Some("main-head".to_string()),
            fork_base_commit: Some("fork".to_string()),
            branch_head: Some("branch-head".to_string()),
            target_head: Some("main-head".to_string()),
            target_head_observed_at: None,
            invariant_violation_count: 0,
            invariant_violations_sample: Vec::new(),
            overlap_count: 0,
            overlap_paths_sample: Vec::new(),
            reasons: Vec::new(),
            server: None,
        }),
        merge_would_succeed: None,
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
        vec![
            "oak switch remote-feature",
            "oak pull",
            "oak merge remote-feature"
        ]
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

    let preview = local_preview(&repo, "feature");
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
        vec![
            "oak fetch".to_string(),
            "oak merge --dry-run feature --json".to_string()
        ]
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

    let preview = local_preview(&repo, "feature");
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

    let preview = local_preview(&repo, "feature");
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

    let preview = local_preview(&repo, "feature");
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

    let preview = local_preview(&repo, "feature");
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

    let preview = local_preview(&repo, "feature");
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

/// Fixture for modify/modify previews: a base commit on main holds
/// `core.rs` with `base_content` (plus an untouched bystander file),
/// then main advances to `parent_content` while `feature` (parented on
/// the base commit) rewrites it to `branch_content`.
fn modify_modify_fixture(
    repo: &SqliteRepository,
    base_content: &str,
    branch_content: &str,
    parent_content: &str,
) {
    let (untouched, _) = manifest_entry(repo, "untouched.txt", "same\n");
    let (base_entry, _) = manifest_entry(repo, "core.rs", base_content);
    let base_manifest_hash = repo
        .put_manifest(vec![untouched.clone(), base_entry])
        .unwrap();
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

    let (main_entry, _) = manifest_entry(repo, "core.rs", parent_content);
    let main_manifest_hash = repo
        .put_manifest(vec![untouched.clone(), main_entry])
        .unwrap();
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

    let (feature_entry, _) = manifest_entry(repo, "core.rs", branch_content);
    let feature_manifest_hash = repo.put_manifest(vec![untouched, feature_entry]).unwrap();
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
}

#[test]
fn merge_preview_reports_clean_modify_modify_as_modified() {
    // fb-28: branch and main both modified the same file in disjoint
    // regions. The content merge is clean, so the predicted merged tree
    // carries the merged file — changed_files must report it as
    // "modified" with sane counts, not as a deletion of the whole file.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let base = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
    let branch = "BRANCH1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
    let parent = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nPARENT8\n";
    modify_modify_fixture(&repo, base, branch, parent);

    let preview = local_preview(&repo, "feature");
    assert!(preview.prediction_available);
    assert_eq!(preview.clean, Some(true));
    assert!(
        preview.conflict_files.is_empty(),
        "disjoint edits must not predict conflicts: {:?}",
        preview.conflict_files
    );
    assert_eq!(preview.changed_file_count, Some(1));
    let file = &preview.changed_files[0];
    assert_eq!(file.path, "core.rs");
    assert_eq!(
        file.status, "modified",
        "clean modify/modify must not be misreported: {file:?}"
    );
    // Relative to main's head the predicted merged tree differs only by
    // the branch's own one-line edit; main's edit is already on the
    // parent side, so nothing close to the whole file is "deleted".
    assert_eq!(file.additions, Some(1));
    assert_eq!(file.deletions, Some(1));
}

#[test]
fn merge_preview_conflicts_are_not_reported_as_deletions() {
    // Overlapping edits: the path belongs in conflict_files and must not
    // leak into changed_files as a phantom deletion.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let base = "line1\nline2\nline3\n";
    let branch = "line1\nBRANCH\nline3\n";
    let parent = "line1\nPARENT\nline3\n";
    modify_modify_fixture(&repo, base, branch, parent);

    let preview = local_preview(&repo, "feature");
    assert!(preview.prediction_available);
    assert_eq!(preview.clean, Some(false));
    assert_eq!(preview.conflict_file_count, 1);
    assert_eq!(preview.conflict_files[0].path, "core.rs");
    assert_eq!(preview.conflict_files[0].conflict_type, "content");
    assert!(
        preview
            .changed_files
            .iter()
            .all(|file| file.path != "core.rs"),
        "predicted-conflict file must not appear in changed_files: {:?}",
        preview.changed_files
    );
    assert_eq!(preview.changed_file_count, Some(0));
}

/// Record a verified observation of the local main head, as a fetch a
/// moment ago would have.
fn mark_main_fresh(repo: &SqliteRepository) {
    repo.set_metadata(oak_core::MetadataKey::RemoteUrl, "http://127.0.0.1:1")
        .unwrap();
    let head = repo.get_branch_head("main").unwrap();
    crate::remote_head::record_verified(repo, head.as_ref());
}

/// The preview a command with no network produces: main as on record.
fn local_preview(repo: &SqliteRepository, branch: &str) -> MergePreviewJson {
    merge_preview_for_branch(repo, branch, &remote_head::on_record(repo).unwrap()).unwrap()
}

/// The preview a command produces after verifying main against the
/// remote in this invocation and finding the local head current.
fn verified_preview(repo: &SqliteRepository, branch: &str) -> MergePreviewJson {
    let target = RemoteHeadObservation {
        branch: "main".to_string(),
        head: repo.get_branch_head("main").unwrap(),
        observed_at: Some(1),
        source: RemoteHeadSource::VerifiedRemote,
    };
    merge_preview_for_branch(repo, branch, &target).unwrap()
}

/// fb-650: no recommendation computed for a branch may act on whatever
/// branch is current. Every command names the branch, is the
/// branch-independent `oak fetch`, or is an `oak pull` that either runs
/// on the branch or follows an explicit switch to it.
#[test]
fn every_merge_next_command_names_its_branch() {
    let readiness = [
        MergeReadiness::Violation,
        MergeReadiness::Clean,
        MergeReadiness::Conflicts,
        MergeReadiness::Uncertified {
            repair_history: false,
            conflicts: false,
        },
        MergeReadiness::Uncertified {
            repair_history: true,
            conflicts: false,
        },
        MergeReadiness::Uncertified {
            repair_history: false,
            conflicts: true,
        },
        MergeReadiness::Uncertified {
            repair_history: true,
            conflicts: true,
        },
    ];
    for readiness in readiness {
        for on_branch in [true, false] {
            for remote in [true, false] {
                let commands = merge_next_commands("topic", on_branch, remote, readiness);
                assert!(!commands.is_empty(), "{readiness:?}");
                let mut switched = on_branch;
                for command in &commands {
                    let names_branch = command.split_whitespace().any(|part| part == "topic");
                    match command.as_str() {
                        "oak fetch" => {}
                        "oak pull" => assert!(
                            switched,
                            "{readiness:?} on_branch={on_branch}: pull before switch: {commands:?}"
                        ),
                        _ => assert!(
                            names_branch,
                            "{readiness:?} on_branch={on_branch} remote={remote}: \
                                 branch-less command {command:?} in {commands:?}"
                        ),
                    }
                    if command == "oak switch topic" {
                        switched = true;
                    }
                }
                let merges = commands
                    .iter()
                    .any(|command| command.starts_with("oak merge topic"));
                assert_eq!(
                    merges,
                    matches!(readiness, MergeReadiness::Clean | MergeReadiness::Conflicts),
                    "only a certified prediction recommends the merge: {commands:?}"
                );
            }
        }
    }
}

/// fb-629: a main verified by an *earlier* command — however recently —
/// is cached evidence. It keeps its `fresh_local` label and no longer
/// certifies; only this invocation's own verification does.
#[test]
fn a_recently_fetched_main_is_cached_evidence_and_never_certifies() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    intentional_deletion_fixture(&repo);
    mark_main_fresh(&repo);

    let preview = local_preview(&repo, "feature");
    let safety = preview.merge_safety.as_ref().unwrap();
    assert_eq!(safety.target_head_source, "fresh_local");
    assert!(!safety.certified);
    assert_eq!(safety.verdict, "uncertified");
    assert_eq!(safety.uncertified_cause, Some("cached_local_target"));
    assert!(safety.target_head_observed_at.is_some());
    assert_eq!(
        preview.recommended_next_commands,
        vec!["oak merge --dry-run feature --json".to_string()],
        "the recommendation is the command that verifies the target head"
    );

    // A failed verification is reported as such, with the reason.
    let target = RemoteHeadObservation {
        source: RemoteHeadSource::Unverified {
            error: "connection refused".to_string(),
        },
        ..remote_head::on_record(&repo).unwrap()
    };
    let preview = merge_preview_for_branch(&repo, "feature", &target).unwrap();
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(!safety.certified);
    assert!(safety
        .reasons
        .iter()
        .any(|reason| reason.contains("connection refused")));
}

/// main: base commit (kept.rs + removed.rs), then advances by adding
/// new_on_main.rs. feature forks at the base commit and intentionally
/// deletes removed.rs.
fn intentional_deletion_fixture(repo: &SqliteRepository) {
    let (kept, _) = manifest_entry(repo, "kept.rs", "kept\n");
    let (removed, _) = manifest_entry(repo, "removed.rs", "removed\n");
    let base_manifest_hash = repo
        .put_manifest(vec![kept.clone(), removed.clone()])
        .unwrap();
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

    let (new_on_main, _) = manifest_entry(repo, "new_on_main.rs", "added after fork\n");
    let main_manifest_hash = repo
        .put_manifest(vec![kept.clone(), removed, new_on_main])
        .unwrap();
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

    let feature_manifest_hash = repo.put_manifest(vec![kept]).unwrap();
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
}

#[test]
fn merge_preview_certifies_intentional_deletion_against_fresh_target() {
    // fb-105 category 2: the branch deleted a file the target has not
    // touched since the fork — a valid contribution, no violation, and
    // the target-only new file survives into the predicted result.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    intentional_deletion_fixture(&repo);
    let preview = verified_preview(&repo, "feature");
    assert_eq!(preview.clean, Some(true));
    assert_eq!(preview.invariant_violations, Some(Vec::new()));
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(safety.certified);
    assert_eq!(safety.verdict, "safe");
    assert_eq!(safety.target_head_source, "live_remote_fetch");
    assert!(safety.target_head_observed_at.is_some());
    assert_eq!(safety.assessed_target, "main");
    assert_eq!(safety.invariant_violation_count, 0);
    let deletion = preview
        .changed_files
        .iter()
        .find(|file| file.path == "removed.rs")
        .expect("intentional deletion appears in changed_files");
    assert_eq!(deletion.status, "deleted");
    assert_eq!(deletion.merge_safety, Some("branch_change"));
    assert!(
        preview
            .changed_files
            .iter()
            .all(|file| file.path != "new_on_main.rs"),
        "target-only file must survive the predicted merge: {:?}",
        preview.changed_files
    );
    assert_eq!(preview.recommended_next_commands, vec!["oak merge feature"]);
}

#[test]
fn merge_preview_marks_both_sides_changed_as_overlap_warning() {
    // fb-105 category 3: both sides changed the same file since the
    // fork; the clean content merge is allowed but flagged as overlap.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let base = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
    let branch = "BRANCH1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
    let parent = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nPARENT8\n";
    modify_modify_fixture(&repo, base, branch, parent);
    let preview = verified_preview(&repo, "feature");
    assert_eq!(preview.clean, Some(true));
    assert_eq!(preview.invariant_violations, Some(Vec::new()));
    let safety = preview.merge_safety.as_ref().unwrap();
    assert_eq!(safety.verdict, "safe");
    assert_eq!(safety.overlap_count, 1);
    assert_eq!(safety.overlap_paths_sample, vec!["core.rs".to_string()]);
    let file = &preview.changed_files[0];
    assert_eq!(file.path, "core.rs");
    assert_eq!(file.merge_safety, Some("overlap"));
    assert!(
        safety
            .reasons
            .iter()
            .any(|reason| reason.contains("core.rs") && reason.contains("both sides")),
        "overlap reason must name the file: {:?}",
        safety.reasons
    );
}

#[test]
fn merge_preview_refuses_to_certify_against_stale_local_target() {
    // fb-105 stale-local-target fixture: without a verified-fresh local
    // main the preview must say so and refuse to certify — never
    // silently classify against a possibly stale target head.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    intentional_deletion_fixture(&repo);
    // No MainLastCheckedAt metadata: local main is unverified.

    let preview = local_preview(&repo, "feature");
    assert_eq!(preview.clean, Some(true));
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(!safety.certified);
    assert_eq!(safety.verdict, "uncertified");
    assert_eq!(safety.uncertified_cause, Some("stale_local_target"));
    assert_eq!(safety.target_head_source, "stale_local");
    assert!(
        safety
            .reasons
            .iter()
            .any(|reason| reason.contains("refusing to certify")),
        "stale target must be named explicitly: {:?}",
        safety.reasons
    );
    assert_eq!(
        preview.recommended_next_commands,
        vec!["oak merge --dry-run feature --json"],
        "an uncertified preview must steer to the verifying dry run, not straight to merge"
    );
}

/// True when a recommended command would actually perform a merge, as
/// opposed to previewing one. `oak merge --dry-run --json` is a
/// read-only prediction and is exactly what an uncertified verdict is
/// supposed to steer back to, so a plain `contains("oak merge")` check
/// cannot express the invariant.
fn is_merge_executing(command: &str) -> bool {
    (command == "oak merge" || command.starts_with("oak merge ")) && !command.contains("--dry-run")
}

#[test]
fn uncertified_conflicted_preview_never_recommends_merge() {
    // fb-105 fail-closed regression: the uncertified-AND-conflicted
    // cell. A stale local target (no MainLastCheckedAt) plus predicted
    // conflicts used to recommend `oak fetch` / `oak pull` / `oak
    // merge` — an actual merge over a prediction that nothing
    // authoritative certified. Refresh, reconcile, then dry-run again.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let base = "line1\nline2\nline3\n";
    let branch = "line1\nBRANCH\nline3\n";
    let parent = "line1\nPARENT\nline3\n";
    modify_modify_fixture(&repo, base, branch, parent);
    // No MainLastCheckedAt metadata: local main is unverified.

    let preview = local_preview(&repo, "feature");
    assert_eq!(preview.clean, Some(false), "the fixture must conflict");
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(!safety.certified);
    assert_eq!(safety.verdict, "uncertified");
    assert_eq!(safety.uncertified_cause, Some("stale_local_target"));
    assert_eq!(
        preview.recommended_next_commands,
        vec![
            "oak switch feature",
            "oak pull",
            "oak merge --dry-run feature --json"
        ],
        "uncertified + conflicted must re-run the dry run, never merge"
    );
    assert!(
        !preview
            .recommended_next_commands
            .iter()
            .any(|command| is_merge_executing(command)),
        "an uncertified preview must not recommend any merge-executing command: {:?}",
        preview.recommended_next_commands
    );
}

#[test]
fn uncertified_preview_never_recommends_merge_for_any_clean_shape() {
    // Generalization of the assertion above over the whole uncertified
    // table: certified=false crossed with clean ∈ {true, false,
    // unknown}, driven through the real preview builder. Every cell
    // must steer back to a dry run; none may execute a merge.
    for (base, branch, parent, expected_clean) in [
        // Disjoint edits: clean content merge, still uncertified.
        (
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n",
            "BRANCH1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n",
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nPARENT8\n",
            Some(true),
        ),
        // Overlapping edits: predicted conflict, uncertified.
        (
            "line1\nline2\nline3\n",
            "line1\nBRANCH\nline3\n",
            "line1\nPARENT\nline3\n",
            Some(false),
        ),
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        modify_modify_fixture(&repo, base, branch, parent);
        // No MainLastCheckedAt metadata: local main is unverified.

        let preview = local_preview(&repo, "feature");
        assert_eq!(preview.clean, expected_clean);
        assert!(!preview.merge_safety.as_ref().unwrap().certified);
        assert!(
            !preview
                .recommended_next_commands
                .iter()
                .any(|command| is_merge_executing(command)),
            "uncertified (clean={expected_clean:?}) must not recommend a merge: {:?}",
            preview.recommended_next_commands
        );
        assert_eq!(
            preview.recommended_next_commands.last().map(String::as_str),
            Some("oak merge --dry-run feature --json"),
            "every uncertified shape must end on a dry run: {:?}",
            preview.recommended_next_commands
        );
    }
}

#[test]
fn live_target_fetch_certifies_without_local_freshness_metadata() {
    // The checkout-free --remote review path fetches the target head
    // live; that authority certifies even without MainLastCheckedAt —
    // but only when the fetch covered exactly the assessed target at
    // exactly the assessed head.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    intentional_deletion_fixture(&repo);
    let main_head = repo.get_branch_head("main").unwrap().unwrap();

    let preview = merge_preview_for_branch_from(
        &repo,
        "feature",
        &merge_safety::TargetDataOrigin::LiveFetched {
            target: "main",
            head: Some(&main_head),
        },
    )
    .unwrap();
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(safety.certified);
    assert_eq!(safety.verdict, "safe");
    assert_eq!(safety.target_head_source, "live_remote_fetch");
    assert_eq!(safety.fetched_target.as_deref(), Some("main"));
    assert_eq!(safety.fetched_target_head, Some(main_head.to_string()));
}

#[test]
fn live_fetch_of_other_target_does_not_certify_recorded_parent() {
    // fb-105 authority binding: the caller fetched --against some other
    // branch, but the preview assesses the branch's RECORDED PARENT
    // (main). The fetch's authority must not transfer — uncertified
    // with an explicit mismatch, never a false live_remote_fetch.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    intentional_deletion_fixture(&repo);
    let other_head = Hash("dd".repeat(32));

    let preview = merge_preview_for_branch_from(
        &repo,
        "feature",
        &merge_safety::TargetDataOrigin::LiveFetched {
            target: "release-branch",
            head: Some(&other_head),
        },
    )
    .unwrap();
    let safety = preview.merge_safety.as_ref().unwrap();
    assert!(!safety.certified);
    assert_eq!(safety.verdict, "uncertified");
    assert_eq!(safety.uncertified_cause, Some("target_authority_mismatch"));
    assert_eq!(safety.target_head_source, "live_fetch_mismatch");
    assert_eq!(safety.assessed_target, "main");
    assert_eq!(safety.fetched_target.as_deref(), Some("release-branch"));
    assert!(
        safety
            .reasons
            .iter()
            .any(|reason| reason.contains("target authority mismatch")
                && reason.contains("release-branch")),
        "mismatch must be named: {:?}",
        safety.reasons
    );
    assert!(
        !preview
            .recommended_next_commands
            .iter()
            .any(|command| command == "oak merge"),
        "an uncertified preview must not recommend a bare merge: {:?}",
        preview.recommended_next_commands
    );
}

fn preview_with_violation(path: &str) -> MergePreviewJson {
    MergePreviewJson {
        schema_version: SCHEMA_VERSION,
        branch: "feature".to_string(),
        parent: Some("main".to_string()),
        branch_head: Some("bb".repeat(32)),
        parent_head: Some("cc".repeat(32)),
        prediction_available: true,
        prediction_method: "local_manifest_three_way",
        clean: Some(true),
        target_risk: TargetRisk::None,
        conflict_file_count: 0,
        conflict_files: Vec::new(),
        changed_file_count: Some(1),
        changed_files: Vec::new(),
        invariant_violations: Some(vec![path.to_string()]),
        merge_safety: Some(merge_safety::MergeSafetyJson {
            assessment_kind: "four_tree",
            certified: false,
            verdict: "do_not_merge",
            uncertified_cause: None,
            target_head_source: "live_remote_fetch",
            assessed_target: "main".to_string(),
            fetched_target: Some("main".to_string()),
            fetched_target_head: Some("cc".repeat(32)),
            fork_base_commit: Some("aa".repeat(32)),
            branch_head: Some("bb".repeat(32)),
            target_head: Some("cc".repeat(32)),
            target_head_observed_at: None,
            invariant_violation_count: 1,
            invariant_violations_sample: vec![path.to_string()],
            overlap_count: 0,
            overlap_paths_sample: Vec::new(),
            reasons: vec![format!("merge invariant violation: '{path}' ...")],
            server: None,
        }),
        merge_would_succeed: Some(false),
        merge_lineage_evidence: LineageJson {
            branch_parent: Some("main".to_string()),
            branch_own_head: None,
            branch_effective_head: None,
            against_head: None,
            comparison_head: None,
            comparison_source: "test".to_string(),
            fork_point: Some("aa".repeat(32)),
            branch_commit_count: 1,
            caveats: Vec::new(),
        },
        caveats: Vec::new(),
        recommended_next_commands: Vec::new(),
    }
}

#[test]
fn triage_turns_invariant_violation_into_do_not_merge() {
    // A predicted invariant violation must produce an explicit blocking
    // verdict: recommendation do_not_merge, reason naming the file and
    // the tree evidence, and vcs_merge_safe forced to false even though
    // the prediction itself reported "clean".
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let (entry, _) = manifest_entry(&repo, "kept.rs", "kept\n");
    let branch_manifest = Manifest::new(vec![entry.clone()]);
    let against_manifest = Manifest::new(vec![entry]);
    let preview = preview_with_violation("src/new_module.rs");

    let changes: Vec<FileChange> = Vec::new();
    let triage = derive_branch_triage(
        &repo,
        BranchTriageInput {
            branch: "feature",
            remote: false,
            branch_commit_count: 1,
            changed_files: &changes,
            branch_manifest: &branch_manifest,
            against_manifest: &against_manifest,
            missing_blob_paths: &[],
            local_snapshot_missing: false,
            against_head_missing: false,
            merge_preview: &preview,
        },
    )
    .unwrap();

    assert_eq!(triage.recommended_action, RecommendedAction::DoNotMerge);
    assert!(
        triage.reason.contains("merge_invariant_violation")
            && triage.reason.contains("src/new_module.rs")
            && triage.reason.contains("fork_base="),
        "reason must name the file and tree evidence: {}",
        triage.reason
    );
    assert_eq!(triage.vcs_merge_safe, Some(false));
    assert!(!triage.merge_allowed);
    assert_eq!(
        review_recommendations(
            "feature",
            false,
            None,
            RecommendedAction::DoNotMerge,
            "oak branch review feature --merge-preview --json",
        ),
        vec!["oak branch review feature --merge-preview --json"]
    );
}

fn preview_uncertified_clean(cause: &'static str) -> MergePreviewJson {
    let mut preview = preview_with_violation("unused.rs");
    preview.invariant_violations = Some(Vec::new());
    let safety = preview.merge_safety.as_mut().unwrap();
    safety.certified = false;
    safety.verdict = "uncertified";
    safety.uncertified_cause = Some(cause);
    safety.target_head_source = "stale_local";
    safety.invariant_violation_count = 0;
    safety.invariant_violations_sample = Vec::new();
    safety.reasons = vec!["refusing to certify".to_string()];
    preview
}

#[test]
fn triage_fails_closed_on_uncertified_prediction() {
    // fb-105 fail-closed: a clean-but-uncertified prediction must never
    // surface as merge-safe — vcs_merge_safe: false, no
    // validate_then_merge, and recommendations steer through a fetch
    // instead of `oak merge`.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let (kept, _) = manifest_entry(&repo, "kept.rs", "kept\n");
    let (changed, _) = manifest_entry(&repo, "feat.rs", "branch\n");
    let branch_manifest = Manifest::new(vec![kept.clone(), changed]);
    let against_manifest = Manifest::new(vec![kept]);
    let preview = preview_uncertified_clean("stale_local_target");

    let changes = vec![FileChange {
        path: "feat.rs".to_string(),
        change_type: ChangeType::Added,
        old_blob_hash: None,
        new_blob_hash: Some(branch_manifest.get("feat.rs").unwrap().blob_hash.clone()),
        old_path: None,
        old_mode: None,
        new_mode: Some(FileMode::Regular),
    }];
    let triage = derive_branch_triage(
        &repo,
        BranchTriageInput {
            branch: "feature",
            remote: false,
            branch_commit_count: 1,
            changed_files: &changes,
            branch_manifest: &branch_manifest,
            against_manifest: &against_manifest,
            missing_blob_paths: &[],
            local_snapshot_missing: false,
            against_head_missing: false,
            merge_preview: &preview,
        },
    )
    .unwrap();

    assert_ne!(
        triage.recommended_action,
        RecommendedAction::ValidateThenMerge,
        "uncertified must never earn validate_then_merge"
    );
    assert_eq!(triage.recommended_action, RecommendedAction::Review);
    assert_eq!(
        triage.vcs_merge_safe,
        Some(false),
        "uncertified must fail closed, not report merge-safe"
    );
    assert!(
        triage.reason.contains("merge_safety_uncertified")
            && triage.reason.contains("stale_local_target"),
        "reason must state certification was not possible and why: {}",
        triage.reason
    );

    // Recommendations from the review surface steer to fetch, never to
    // `oak merge`.
    let review_preview = review_merge_preview_json(
        preview_uncertified_clean("stale_local_target"),
        &[],
        None,
        0,
        None,
    );
    let commands = review_recommendations(
        "feature",
        false,
        Some(&review_preview),
        triage.recommended_action,
        "oak branch review feature --merge-preview --json",
    );
    // fb-338: never a mutating `oak fetch` as the default; with no
    // remote configured nothing can add evidence, so nothing is named.
    assert!(commands.is_empty(), "{commands:?}");
    let configured = review_recommendations_with_remote(
        "feature",
        false,
        true,
        Some(&review_preview),
        triage.recommended_action,
        "oak branch review feature --merge-preview --json",
    );
    assert_eq!(
        configured,
        vec!["oak merge --dry-run feature --json".to_string()]
    );
    assert!(
        !commands.iter().any(|command| command.contains("oak merge")),
        "uncertified recommendations must never include a merge: {commands:?}"
    );
}

/// A preview that claims a prediction ran (`prediction_available:
/// true`, `clean: Some(true)`) but carries no merge_safety block at
/// all. Real previews always attach the block; only a fixture (or a
/// future regression) can produce this shape — which is exactly why it
/// must fail closed.
fn preview_claimed_without_safety_block() -> MergePreviewJson {
    let mut preview = preview_with_violation("unused.rs");
    preview.invariant_violations = None;
    preview.merge_safety = None;
    preview
}

#[test]
fn triage_fails_closed_when_claimed_prediction_has_no_safety_block() {
    // fb-105 hardening: prediction_available: true with an ABSENT
    // merge_safety block must never be inferred as certified safe —
    // vcs_merge_safe: false, uncertified review verdict, and no merge
    // recommendation anywhere.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let (kept, _) = manifest_entry(&repo, "kept.rs", "kept\n");
    let (changed, _) = manifest_entry(&repo, "feat.rs", "branch\n");
    let branch_manifest = Manifest::new(vec![kept.clone(), changed]);
    let against_manifest = Manifest::new(vec![kept]);
    let preview = preview_claimed_without_safety_block();
    assert!(preview.prediction_available);
    assert!(preview.merge_safety.is_none());

    let changes = vec![FileChange {
        path: "feat.rs".to_string(),
        change_type: ChangeType::Added,
        old_blob_hash: None,
        new_blob_hash: Some(branch_manifest.get("feat.rs").unwrap().blob_hash.clone()),
        old_path: None,
        old_mode: None,
        new_mode: Some(FileMode::Regular),
    }];
    let triage = derive_branch_triage(
        &repo,
        BranchTriageInput {
            branch: "feature",
            remote: false,
            branch_commit_count: 1,
            changed_files: &changes,
            branch_manifest: &branch_manifest,
            against_manifest: &against_manifest,
            missing_blob_paths: &[],
            local_snapshot_missing: false,
            against_head_missing: false,
            merge_preview: &preview,
        },
    )
    .unwrap();

    assert_eq!(
        triage.vcs_merge_safe,
        Some(false),
        "absent merge_safety block with a claimed prediction must fail closed"
    );
    assert_ne!(
        triage.recommended_action,
        RecommendedAction::ValidateThenMerge
    );
    assert_eq!(triage.recommended_action, RecommendedAction::Review);
    assert!(
        triage.reason.contains("merge_safety_uncertified")
            && triage.reason.contains("merge_safety_block_missing"),
        "review-surface verdict must state the uncertified cause: {}",
        triage.reason
    );

    // The recommendation surface also refuses to steer to a merge.
    let review_preview =
        review_merge_preview_json(preview_claimed_without_safety_block(), &[], None, 0, None);
    let commands = review_recommendations(
        "feature",
        false,
        Some(&review_preview),
        triage.recommended_action,
        "oak branch review feature --merge-preview --json",
    );
    // fb-338: never a mutating `oak fetch` as the default; with no
    // remote configured nothing can add evidence, so nothing is named.
    assert!(commands.is_empty(), "{commands:?}");
    let configured = review_recommendations_with_remote(
        "feature",
        false,
        true,
        Some(&review_preview),
        triage.recommended_action,
        "oak branch review feature --merge-preview --json",
    );
    assert_eq!(
        configured,
        vec!["oak merge --dry-run feature --json".to_string()]
    );
    assert!(
        !commands.iter().any(|command| command.contains("oak merge")),
        "absent-block preview must never yield a merge recommendation: {commands:?}"
    );
}

#[test]
fn review_verdict_cannot_be_overridden_by_a_clean_certified_preview() {
    let preview = merge_preview(Some(true));
    for action in [RecommendedAction::Review, RecommendedAction::Unknown] {
        for remote in [false, true] {
            let command = "oak branch review feature --merge-preview --json";
            assert_eq!(
                review_recommendations("feature", remote, Some(&preview), action, command),
                vec![command.to_string()],
            );
        }
    }
}

#[test]
fn review_recommendations_never_merge_for_any_uncertified_shape() {
    // fb-105 fail-closed, generalized across every NOT-POSITIVELY-
    // CERTIFIED shape: an explicit uncertified verdict, a claimed
    // prediction whose merge_safety block is absent, and a preview
    // where no prediction ran at all (`prediction_available: false`,
    // no block, no violation list — the ordinary partially-fetched
    // repo). Crossed with clean in {true, false, unknown} x every
    // triage action that reaches the preview-gated logic x
    // local/--remote. No cell may recommend a command that merges.
    //
    // The no_prediction shape is the one the old "not uncertified"
    // test let through: it is not an uncertified *verdict*, it is the
    // absence of any verdict, which is weaker evidence, not stronger.
    for clean in [Some(true), Some(false), None] {
        let mut explicit = merge_preview(clean);
        let safety = explicit.merge_safety.as_mut().unwrap();
        safety.certified = false;
        safety.verdict = "uncertified";
        safety.uncertified_cause = Some("stale_local_target");
        safety.target_head_source = "stale_local";

        let mut absent_block = merge_preview(clean);
        absent_block.merge_safety = None;

        // Deliberately keeps the loop's `clean` value, including
        // `Some(true)`: a preview that claims a clean merge while
        // admitting no prediction ran must still never merge.
        let mut no_prediction = merge_preview(clean);
        no_prediction.prediction_available = false;
        no_prediction.merge_safety = None;
        no_prediction.invariant_violations = None;

        for (shape_name, preview) in [
            ("explicit", &explicit),
            ("absent_block", &absent_block),
            ("no_prediction", &no_prediction),
        ] {
            for action in [
                RecommendedAction::ValidateThenMerge,
                RecommendedAction::Resolve,
                RecommendedAction::Review,
                RecommendedAction::Unknown,
            ] {
                for remote in [false, true] {
                    let commands = review_recommendations(
                        "remote-feature",
                        remote,
                        Some(preview),
                        action,
                        "oak branch review remote-feature --merge-preview --json",
                    );
                    assert!(
                        !commands.iter().any(|command| is_merge_executing(command)),
                        "uncertified ({shape_name}, clean={clean:?}, action={action:?}, \
                             remote={remote}) must never recommend a merge: {commands:?}"
                    );
                    // Never the identical command (a remote re-run
                    // cannot acquire more) and never a bare `oak fetch`.
                    let expected = if remote {
                        "# bounded remote evidence could not certify this merge; \
                             `oak fetch` performs a full ancestry walk and updates local \
                             refs, after which run: oak branch review remote-feature \
                             --merge-preview --json"
                    } else {
                        ""
                    };
                    let expected: Vec<String> = if expected.is_empty() {
                        Vec::new()
                    } else {
                        vec![expected.to_string()]
                    };
                    assert_eq!(
                        commands, expected,
                        "uncertified ({shape_name}, clean={clean:?}, action={action:?}, \
                             remote={remote}) must steer through a fetch and a re-review"
                    );
                }
            }
        }
    }
}

/// `branch_lineage` (what fork-base callers — status --json, `oak diff
/// --branch`, `oak owners --branch`, the status fingerprint — resolve
/// through) probes each side's commit and root tree instead of flattening
/// it. This pins exactly which "incomplete locally" caveats survive that:
/// a missing commit or root tree is still reported, identically to
/// `branch_comparison`; a directory missing below the root is not, since
/// only a full flatten finds it. Merge prediction and `merge --dry-run`
/// keep using `branch_comparison`, so they still see the deep gap.
#[test]
fn branch_lineage_keeps_shallow_incomplete_caveats_and_drops_deep_ones() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let commit = |branch: &str, parent: Option<&Hash>, manifest: Hash| {
        repo.put_commit(
            branch.to_string(),
            parent.cloned(),
            None,
            manifest,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap()
    };

    let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
    let (nested, _) = manifest_entry(&repo, "dir/sub/x.txt", "x\n");
    let base_tree = repo
        .put_manifest(vec![base_entry.clone(), nested.clone()])
        .unwrap();
    let base = commit("main", None, base_tree.clone());
    let (other, _) = manifest_entry(&repo, "other.txt", "other\n");
    let other_tree = repo.put_manifest(vec![base_entry.clone(), other]).unwrap();
    let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
    let feature_tree = repo
        .put_manifest(vec![base_entry.clone(), nested, feature_entry])
        .unwrap();
    let feature = commit("feature", Some(&base), feature_tree);

    // Main moved on; of its new snapshot only the root tree is stored.
    let (moved, _) = manifest_entry(&repo, "dir/sub/y.txt", "y\n");
    let built = oak_core::build_tree(&[base_entry, moved]).unwrap();
    let root = built
        .trees
        .iter()
        .find(|tree| tree.hash == built.root_hash)
        .unwrap();
    repo.store_tree(root).unwrap();
    let main_missing_subtree = commit("main", Some(&base), built.root_hash.clone());
    let main_missing_root = commit("main", Some(&base), Hash("ab".repeat(32)));
    let main_missing_commit = commit("main", Some(&base), other_tree);
    let feature_missing_root = commit("feature", Some(&base), Hash("ef".repeat(32)));

    repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    let both = |main_head: &Hash, feature_head: &Hash| {
        repo.set_branch_head("main", main_head).unwrap();
        repo.set_branch_head("feature", feature_head).unwrap();
        if main_head == &main_missing_commit {
            // The head row outlives its commit, as after a partial fetch.
            let raw = rusqlite::Connection::open(temp.path().join("oak.db")).unwrap();
            raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            raw.execute("DELETE FROM commits WHERE hash = ?1", [main_head.as_str()])
                .unwrap();
        }
        (
            branch_comparison(&repo, "feature", "main").unwrap(),
            branch_lineage(&repo, "feature", "main").unwrap(),
        )
    };
    const AGAINST: &str = "Comparison head for branch 'main' is incomplete locally";
    const BRANCH: &str = "Branch 'feature' snapshot is incomplete locally";
    let mentions = |caveats: &[String], needle: &str| caveats.iter().any(|c| c.contains(needle));

    // Intact: nothing to report either way.
    let (full, lineage) = both(&base, &feature);
    assert!(full.caveats.is_empty(), "{:?}", full.caveats);
    assert!(lineage.caveats.is_empty(), "{:?}", lineage.caveats);
    assert_eq!(lineage.fork_point, Some(base.clone()));

    // A directory missing below the root: only the full flatten sees it.
    let (full, lineage) = both(&main_missing_subtree, &feature);
    assert!(mentions(&full.caveats, AGAINST), "{:?}", full.caveats);
    assert!(!full.comparison_snapshot_available);
    assert!(lineage.caveats.is_empty(), "{:?}", lineage.caveats);
    assert!(lineage.comparison_snapshot_available);
    assert_eq!(lineage.fork_point, full.fork_point);
    assert_eq!(lineage.fork_point, Some(base.clone()));

    // Missing root tree, on either side: reported identically.
    let (full, lineage) = both(&main_missing_root, &feature);
    assert!(mentions(&lineage.caveats, AGAINST), "{:?}", lineage.caveats);
    assert_eq!(lineage.caveats, full.caveats);
    assert!(!lineage.comparison_snapshot_available);
    assert_eq!(lineage.fork_point, full.fork_point);

    let (full, lineage) = both(&base, &feature_missing_root);
    assert!(mentions(&lineage.caveats, BRANCH), "{:?}", lineage.caveats);
    assert_eq!(lineage.caveats, full.caveats);
    assert!(!lineage.branch_snapshot_available);

    // Missing commit: reported identically (with the lineage caveat too).
    let (full, lineage) = both(&main_missing_commit, &feature);
    assert!(mentions(&lineage.caveats, AGAINST), "{:?}", lineage.caveats);
    assert_eq!(lineage.caveats, full.caveats);
    assert_eq!(lineage.fork_point, full.fork_point);
    assert_eq!(lineage.fork_point, None);
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

    assert_eq!(computed.changed_file_count, 1);
    assert_eq!(computed.changed_files[0].path, "feature.txt");
    assert_eq!(
        computed.lineage_comparison_source,
        "net_merge_unavailable_tree_fallback"
    );
    assert!(
        computed
            .extra_caveats
            .iter()
            .any(|caveat| caveat.contains("ancestry is incomplete locally")),
        "expected incomplete ancestry caveat, got {:?}",
        computed.extra_caveats
    );
    assert!(
        computed
            .extra_caveats
            .iter()
            .any(|caveat| caveat.contains("branch tree diff")),
        "expected tree fallback caveat, got {:?}",
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
    assert_eq!(
        computed.lineage_comparison_source,
        "net_merge_unavailable_tree_fallback"
    );
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
    assert_eq!(
        computed.lineage_comparison_source,
        "net_merge_unavailable_tree_fallback"
    );
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
    assert_eq!(
        computed.lineage_comparison_source,
        "net_merge_unavailable_tree_fallback"
    );
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

    assert_eq!(computed.changed_file_count, 2);
    let paths: Vec<&str> = computed
        .changed_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert!(paths.contains(&"feature.txt"), "got {paths:?}");
    assert!(paths.contains(&"main.txt"), "got {paths:?}");
    assert_eq!(
        computed.lineage_comparison_source,
        "net_merge_unavailable_tree_fallback"
    );
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
        let computed = compute_branch_diff(&repo, &comparison, "feature", "main", mode).unwrap();
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

    let (_comparison, triage) = branch_triage_evidence(
        &repo,
        "feature",
        "main",
        false,
        false,
        &crate::remote_head::on_record(&repo).unwrap(),
    )
    .unwrap();

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
    assert!(!summary.binary_or_large);
    assert_eq!(summary.content_unavailable_reason, Some("missing_blob"));
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
        content_unavailable_reason: None,
        path: path.to_string(),
        status: "modified",
        old_path: None,
        additions: None,
        deletions: None,
        binary_or_large: false,
        stats_available: true,
        category: None,
        patch: None,
        hunks: None,
        basis: None,
        patch_omitted,
        patch_omitted_reason: patch_omitted.then_some(OMITTED_BYTE_BUDGET),
        merge_safety: None,
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

#[test]
fn remote_comparison_uses_pinned_heads_not_matching_local_refs() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let make_commit = |branch: &str, content: &str| {
        let (entry, _) = manifest_entry(&repo, "tracked.txt", content);
        let manifest = repo.put_manifest(vec![entry]).unwrap();
        repo.put_commit(
            branch.to_string(),
            None,
            None,
            manifest,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap()
    };
    let remote_source = make_commit("feature", "remote source\n");
    let remote_target = make_commit("main", "remote target\n");
    let local_source = make_commit("feature", "local unpushed\n");
    let local_target = make_commit("main", "local target\n");
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("feature", &local_source).unwrap();
    repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.set_branch_head("main", &local_target).unwrap();

    let comparison = remote_branch_comparison(
        &repo,
        &RemoteAnalysisBranch {
            initialization_target: false,
            name: "feature".to_string(),
            head: Some(remote_source.clone()),
            parent: Some("main".to_string()),
        },
        &RemoteAnalysisBranch {
            initialization_target: false,
            name: "main".to_string(),
            head: Some(remote_target.clone()),
            parent: None,
        },
    )
    .unwrap();

    assert_eq!(comparison.branch_head, Some(remote_source));
    assert_eq!(comparison.against_head, Some(remote_target));
    assert_ne!(comparison.branch_head, Some(local_source));
    assert_ne!(comparison.against_head, Some(local_target));
}

#[test]
fn remote_commit_count_storage_failure_cannot_certify_empty() {
    let walk = ReviewAncestorWalk {
        ancestors: vec![Hash("11".repeat(32))],
        ..Default::default()
    };
    let result = count_remote_branch_commits(&walk, "feature", |_| {
        Err(OakError::Database("count read failed".into()))
    });
    assert!(matches!(result, Err(OakError::Database(message)) if message == "count read failed"));
}

#[test]
fn remote_commit_count_disappearing_commit_cannot_certify_empty() {
    let hash = Hash("22".repeat(32));
    let walk = ReviewAncestorWalk {
        ancestors: vec![hash.clone()],
        ..Default::default()
    };
    let result = count_remote_branch_commits(&walk, "feature", |_| Ok(None));
    assert!(
        matches!(result, Err(OakError::IncompleteCommitData { missing, .. }) if missing == hash.to_string())
    );
}

#[test]
fn remote_commit_count_missing_history_cannot_certify_empty() {
    let walk = ReviewAncestorWalk {
        missing_edges: vec![Hash("33".repeat(32))],
        ..Default::default()
    };
    let result = count_remote_branch_commits(&walk, "feature", |_| unreachable!());
    assert!(matches!(result, Err(OakError::IncompleteAncestry { .. })));
}

#[test]
fn remote_positive_commit_count_with_missing_history_is_an_explicit_lower_bound() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let manifest = repo.put_manifest(vec![]).unwrap();
    let head = repo
        .put_commit(
            "feature".to_string(),
            Some(Hash("44".repeat(32))),
            None,
            manifest,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    let comparison = remote_branch_comparison(
        &repo,
        &RemoteAnalysisBranch {
            initialization_target: false,
            name: "feature".to_string(),
            head: Some(head.clone()),
            parent: Some("main".to_string()),
        },
        &RemoteAnalysisBranch {
            initialization_target: false,
            name: "main".to_string(),
            head: Some(head),
            parent: None,
        },
    )
    .unwrap();
    assert_eq!(comparison.branch_commit_count, 1);
    assert!(comparison
        .caveats
        .iter()
        .any(|caveat| caveat.contains("branch_commit_count is a lower bound")));
}
