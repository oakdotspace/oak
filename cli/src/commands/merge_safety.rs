//! Four-tree merge-safety classification for merge previews (fb-105).
//!
//! Every merge prediction is judged against **four trees**:
//!
//! 1. **FORK BASE** — the manifest at the branch's fork point from the
//!    target: the commit `merge_lineage_evidence.fork_point` names, resolved
//!    by [`crate::commands::review::branch_comparison`] via its
//!    `first_common_ancestor` walk. This is the authoritative "what did the
//!    branch start from" tree, computed independently of whatever base the
//!    merge machinery used.
//! 2. **BRANCH HEAD** — the manifest at the branch's effective head.
//! 3. **CURRENT TARGET HEAD** — the manifest at the target's (parent's)
//!    head. Only authoritative when it comes from a live remote fetch **of
//!    that exact target at that exact head** or verified-fresh local data
//!    (`MetadataKey::MainLastCheckedAt` within
//!    [`TARGET_FRESHNESS_TTL_SECS`]). When only stale local target data is
//!    available — or a live fetch covered a different target than the one
//!    assessed — the assessment says so and refuses to certify. It never
//!    silently classifies against a stale or unverified base.
//! 4. **PREDICTED RESULT** — the three-way merge output the preview
//!    computed (clean entries plus content-merge-resolved entries).
//!
//! The merge invariant: **a path the target changed since the fork and the
//! branch never touched must survive unchanged into the predicted result.**
//! A predicted result that drops or reverts such a path is a merge invariant
//! violation — the fb-105 / fb-7d2bbb failure shape, where a "clean" preview
//! produced a merged tree deleting current-main-only files the branch never
//! touched. Detection lives here (CLI preview surfaces); server-side
//! enforcement at squash-merge time is specified in `docs/merge-safety.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{FileMode, Hash, Manifest, MetadataKey, Repository, DEFAULT_BRANCH};
use serde::Serialize;

/// How recently local `main` must have been verified against the remote
/// (`MetadataKey::MainLastCheckedAt`) for local target data to count as
/// fresh. Matches the `oak switch -c` "recently refreshed" window.
pub(crate) const TARGET_FRESHNESS_TTL_SECS: u64 = 60;

/// Every list that repeats path names outside the canonical top-level
/// `invariant_violations` field — the one place this local classification's
/// complete violation list appears — (samples, reasons, caveats, triage
/// reasons, error messages) is bounded to this many entries plus a total
/// count.
pub(crate) const PATH_SAMPLE_LIMIT: usize = 5;

/// Per-path classification of a difference between the CURRENT TARGET HEAD
/// tree and the PREDICTED RESULT tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeSafetyClass {
    /// The branch did not touch this path since the fork, yet the predicted
    /// result does not preserve the target's current entry (the entry
    /// disappears or reverts). Merging would destroy target-side work the
    /// branch never changed — hard-fail.
    InvariantViolation,
    /// Branch-only change since the fork (including an intentional deletion)
    /// carried into the result while the target stayed unchanged. Valid.
    BranchChange,
    /// Both sides changed the path since the fork and the prediction
    /// content-merged them (or took one side). Needs acknowledgement, not a
    /// hard failure.
    Overlap,
}

impl MergeSafetyClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvariantViolation => "invariant_violation",
            Self::BranchChange => "branch_change",
            Self::Overlap => "overlap",
        }
    }
}

/// Where the CURRENT TARGET HEAD tree came from, i.e. how much authority the
/// classification can claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetHeadSource {
    /// The exact assessed target (name and head hash) was fetched from the
    /// remote in this invocation.
    LiveRemoteFetch,
    /// Local target data verified against the remote within
    /// [`TARGET_FRESHNESS_TTL_SECS`].
    FreshLocal,
    /// The target is a local (non-`main`) branch whose local head is the
    /// only authority there is.
    LocalBranchHead,
    /// Local-only target data with no recent remote verification. The
    /// assessment must say so and refuse to certify.
    StaleLocal,
    /// A live fetch happened, but it covered a different target (or a
    /// different head) than the one this assessment classified against —
    /// the fetch's authority does not transfer. Refuse to certify.
    LiveFetchMismatch,
}

impl TargetHeadSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LiveRemoteFetch => "live_remote_fetch",
            Self::FreshLocal => "fresh_local",
            Self::LocalBranchHead => "local_branch_head",
            Self::StaleLocal => "stale_local",
            Self::LiveFetchMismatch => "live_fetch_mismatch",
        }
    }

    fn certifiable(self) -> bool {
        !matches!(self, Self::StaleLocal | Self::LiveFetchMismatch)
    }
}

/// How the caller obtained its target data, before it is bound to the
/// specific target the assessment classifies against.
pub(crate) enum TargetDataOrigin<'a> {
    /// No remote round-trip happened; freshness is judged from local
    /// metadata ([`local_target_head_source`]).
    LocalOnly,
    /// The caller fetched `target` (at `head`, when the remote had one)
    /// from the remote in this invocation. Authority transfers only to an
    /// assessment of exactly this `(target, head)` pair.
    LiveFetched {
        target: &'a str,
        head: Option<&'a Hash>,
    },
}

/// The target-head authority for one assessment: the resolved source plus
/// the fetched `(target, head)` binding it was derived from, so the JSON can
/// record which target the certification actually covers.
pub(crate) struct TargetAuthority {
    pub(crate) source: TargetHeadSource,
    pub(crate) fetched_target: Option<String>,
    pub(crate) fetched_target_head: Option<String>,
    /// Human-readable description of a fetch/assessment mismatch, when one
    /// was detected.
    pub(crate) mismatch: Option<String>,
}

/// Bind a [`TargetDataOrigin`] to the target this assessment classifies
/// against (`assessed_target` at `assessed_head`).
///
/// A live fetch certifies only when it fetched exactly the assessed target
/// name **and** the assessed head hash; anything else is a mismatch and
/// refuses to certify. Local-only origins fall back to metadata freshness.
pub(crate) fn resolve_target_authority(
    repo: &dyn Repository,
    origin: &TargetDataOrigin<'_>,
    assessed_target: &str,
    assessed_head: &Hash,
) -> oak_core::Result<TargetAuthority> {
    match origin {
        TargetDataOrigin::LocalOnly => Ok(TargetAuthority {
            source: local_target_head_source(repo, assessed_target)?,
            fetched_target: None,
            fetched_target_head: None,
            mismatch: None,
        }),
        TargetDataOrigin::LiveFetched { target, head } => {
            let fetched_target = Some((*target).to_string());
            let fetched_target_head = head.map(ToString::to_string);
            if *target == assessed_target && *head == Some(assessed_head) {
                Ok(TargetAuthority {
                    source: TargetHeadSource::LiveRemoteFetch,
                    fetched_target,
                    fetched_target_head,
                    mismatch: None,
                })
            } else {
                let fetched_head_short = head
                    .map(|hash| hash.short().to_string())
                    .unwrap_or_else(|| "no head".to_string());
                Ok(TargetAuthority {
                    source: TargetHeadSource::LiveFetchMismatch,
                    fetched_target,
                    fetched_target_head,
                    mismatch: Some(format!(
                        "the live fetch covered '{target}' ({fetched_head_short}) but this \
                         assessment classified against '{assessed_target}' ({})",
                        assessed_head.short(),
                    )),
                })
            }
        }
    }
}

/// Assess the freshness of local data for `target` without any network I/O.
///
/// `main` is server-owned, so local `main` counts as fresh only when the
/// last verified fetch (`MainLastCheckedAt`) is recent. Any other target is
/// a local branch whose local head is authoritative by definition.
pub(crate) fn local_target_head_source(
    repo: &dyn Repository,
    target: &str,
) -> oak_core::Result<TargetHeadSource> {
    if target != DEFAULT_BRANCH {
        return Ok(TargetHeadSource::LocalBranchHead);
    }
    let Some(raw) = repo.get_metadata(MetadataKey::MainLastCheckedAt)? else {
        return Ok(TargetHeadSource::StaleLocal);
    };
    let Ok(checked_at) = raw.parse::<u64>() else {
        return Ok(TargetHeadSource::StaleLocal);
    };
    if unix_now_secs().saturating_sub(checked_at) <= TARGET_FRESHNESS_TTL_SECS {
        Ok(TargetHeadSource::FreshLocal)
    } else {
        Ok(TargetHeadSource::StaleLocal)
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// The merge-safety block emitted inside merge-preview JSON payloads.
///
/// The **canonical** violation list lives once in the payload's top-level
/// `invariant_violations` field; this block carries the count plus a bounded
/// sample ([`PATH_SAMPLE_LIMIT`]) so large violation sets don't repeat
/// unbounded across fields.
///
/// That top-level list is **complete for this local classification**: `oak`
/// classifies the whole predicted tree from local data and applies no cap
/// and no path policy, so every path it judged a violation is in it.
/// Completeness is a property of the local classifier, NOT of the field
/// name: oakspace's checkout-free review API returns a same-named field that
/// is explicitly *not* complete — capped at 10,000 paths, path-policy
/// filtered, with an `invariant_violations_truncated` companion flag and a
/// server-issued `violations_digest` binding the complete pre-cap,
/// pre-filter sets. See `docs/merge-safety.md`, "Same field name, different
/// guarantee". Never carry the local guarantee over to a server response.
#[derive(Debug, Serialize)]
pub(crate) struct MergeSafetyJson {
    /// True only when the four-tree classification ran against an
    /// authoritative target head and found no invariant violations.
    pub(crate) certified: bool,
    /// "safe" | "do_not_merge" | "uncertified".
    pub(crate) verdict: &'static str,
    /// Why certification was not possible, when it wasn't:
    /// "stale_local_target" | "fork_base_unavailable" |
    /// "target_authority_mismatch".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) uncertified_cause: Option<&'static str>,
    pub(crate) target_head_source: &'static str,
    /// The target branch this assessment classified against.
    pub(crate) assessed_target: String,
    /// The target the live fetch actually covered, when one happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fetched_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fetched_target_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fork_base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_head: Option<String>,
    /// Total number of invariant violations. The full path list is in the
    /// payload's top-level `invariant_violations` field.
    pub(crate) invariant_violation_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) invariant_violations_sample: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) overlap_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) overlap_paths_sample: Vec<String>,
    /// Human-readable sentences naming flagged files (bounded sample) and
    /// the tree evidence that produced the classification.
    pub(crate) reasons: Vec<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Everything the four-tree assessment needs. All manifests are borrowed;
/// the assessment does no I/O.
pub(crate) struct MergeSafetyInput<'a> {
    /// FORK BASE manifest; `None` when the fork point could not be resolved
    /// (classification is then impossible and the assessment refuses to
    /// certify).
    pub(crate) fork_base: Option<&'a Manifest>,
    /// BRANCH HEAD manifest.
    pub(crate) branch: &'a Manifest,
    /// CURRENT TARGET HEAD manifest.
    pub(crate) target: &'a Manifest,
    /// PREDICTED RESULT manifest.
    pub(crate) predicted: &'a Manifest,
    /// Paths the prediction reports as conflicts — excluded from
    /// classification because they are already surfaced, and never silently
    /// land.
    pub(crate) predicted_conflicts: &'a HashSet<String>,
    /// The target branch name this assessment classifies against.
    pub(crate) assessed_target: &'a str,
    pub(crate) authority: TargetAuthority,
    pub(crate) fork_base_commit: Option<&'a Hash>,
    pub(crate) branch_head: Option<&'a Hash>,
    pub(crate) target_head: Option<&'a Hash>,
}

pub(crate) struct MergeSafetyAssessment {
    pub(crate) json: MergeSafetyJson,
    /// This local classification's complete violation list (sorted): no cap,
    /// no path filtering. Serialized once, as the payload's canonical
    /// top-level `invariant_violations` field — whose server-side namesake
    /// is capped and filtered instead (see [`MergeSafetyJson`]).
    pub(crate) invariant_violations: Vec<String>,
    /// Per-path classes for stamping onto `changed_files` summaries.
    pub(crate) per_file: BTreeMap<String, MergeSafetyClass>,
}

fn sample(paths: &[String]) -> Vec<String> {
    paths.iter().take(PATH_SAMPLE_LIMIT).cloned().collect()
}

/// "a, b, c" for small lists; "a, b, c, d, e, … and N more" past
/// [`PATH_SAMPLE_LIMIT`]. The single bounded rendering used everywhere a
/// path list leaves the canonical top-level `invariant_violations` field —
/// which is where this local classification's complete list stays.
pub(crate) fn bounded_path_list(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(PATH_SAMPLE_LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let rest = paths.len().saturating_sub(PATH_SAMPLE_LIMIT);
    if rest == 0 {
        shown
    } else {
        format!("{shown}, … and {rest} more")
    }
}

/// Classify every path where the PREDICTED RESULT differs from the CURRENT
/// TARGET HEAD, judged against the FORK BASE and BRANCH HEAD trees.
///
/// Paths the prediction already reports as conflicts are skipped. Paths
/// where the predicted entry equals the target entry are safe by
/// construction (the target's current state survives) and are not
/// classified.
///
/// Complexity: one linear pass over each manifest builds a path-indexed
/// table of the four signatures (`Manifest::get` is a linear scan, so
/// per-path lookups would be quadratic in repo size); classification then
/// visits each distinct path once. Total O(N) expected over the summed
/// entry counts, plus O(V log V) to order the (small) classified set.
pub(crate) fn classify_predicted_merge(
    fork_base: &Manifest,
    branch: &Manifest,
    target: &Manifest,
    predicted: &Manifest,
    predicted_conflicts: &HashSet<String>,
) -> BTreeMap<String, MergeSafetyClass> {
    type Sig<'a> = Option<(&'a Hash, FileMode)>;
    let total: usize = [fork_base, branch, target, predicted]
        .iter()
        .map(|manifest| manifest.entries.len())
        .sum();
    // Slots: [fork_base, branch, target, predicted] signatures per path.
    let mut by_path: HashMap<&str, [Sig<'_>; 4]> = HashMap::with_capacity(total);
    for (slot, manifest) in [fork_base, branch, target, predicted]
        .into_iter()
        .enumerate()
    {
        for entry in &manifest.entries {
            by_path.entry(entry.path.as_str()).or_default()[slot] =
                Some((&entry.blob_hash, entry.mode));
        }
    }

    let mut classes = BTreeMap::new();
    for (path, [base_sig, branch_sig, target_sig, predicted_sig]) in by_path {
        if predicted_conflicts.contains(path) {
            continue;
        }
        if predicted_sig == target_sig {
            // The target's current state survives the merge untouched.
            continue;
        }

        let branch_changed = branch_sig != base_sig;
        let target_changed = target_sig != base_sig;

        let class = match (branch_changed, target_changed) {
            // Branch-only change (edit, add, or intentional deletion)
            // carried into the result: valid.
            (true, false) => MergeSafetyClass::BranchChange,
            // Both sides changed since the fork; the prediction reconciled
            // them: ambiguous overlap, needs acknowledgement.
            (true, true) => MergeSafetyClass::Overlap,
            // The branch never touched this path since the fork, yet the
            // predicted result does not preserve the target's entry: the
            // merge would destroy target-side state. This covers both the
            // target-only-change case (fb-105) and the phantom case where
            // neither side changed but the result differs anyway.
            (false, _) => MergeSafetyClass::InvariantViolation,
        };
        classes.insert(path.to_string(), class);
    }
    classes
}

/// Run the four-tree assessment and build the JSON block, verdict, and
/// per-file classes.
pub(crate) fn assess(input: MergeSafetyInput<'_>) -> MergeSafetyAssessment {
    let evidence = format!(
        "fork_base={}, branch_head={}, target_head={} ({})",
        input
            .fork_base_commit
            .map(|hash| hash.short().to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        input
            .branch_head
            .map(|hash| hash.short().to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        input
            .target_head
            .map(|hash| hash.short().to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        input.authority.source.as_str(),
    );

    let mut reasons = Vec::new();
    let (per_file, invariant_violations, overlap_paths) = match input.fork_base {
        Some(fork_base) => {
            let per_file = classify_predicted_merge(
                fork_base,
                input.branch,
                input.target,
                input.predicted,
                input.predicted_conflicts,
            );
            let mut violations = Vec::new();
            let mut overlaps = Vec::new();
            for (path, class) in &per_file {
                match class {
                    MergeSafetyClass::InvariantViolation => {
                        // One reason per violating file, bounded: the full
                        // list lives once in invariant_violations.
                        if violations.len() < PATH_SAMPLE_LIMIT {
                            let fate = if input.predicted.get(path).is_none() {
                                "deletes it"
                            } else {
                                "reverts or alters it"
                            };
                            reasons.push(format!(
                                "merge invariant violation: '{path}' is unchanged on the branch since the fork point \
                                 but differs on the current target, and the predicted merge result {fate} \
                                 (evidence: {evidence})"
                            ));
                        }
                        violations.push(path.clone());
                    }
                    MergeSafetyClass::Overlap => overlaps.push(path.clone()),
                    MergeSafetyClass::BranchChange => {}
                }
            }
            if violations.len() > PATH_SAMPLE_LIMIT {
                reasons.push(format!(
                    "… and {} more invariant violation(s); the complete path list is in \
                     invariant_violations",
                    violations.len() - PATH_SAMPLE_LIMIT
                ));
            }
            if !overlaps.is_empty() {
                reasons.push(format!(
                    "both sides changed since the fork point (content-merged, acknowledge before merging): {} \
                     (evidence: {evidence})",
                    bounded_path_list(&overlaps)
                ));
            }
            (per_file, violations, overlaps)
        }
        None => {
            reasons.push(format!(
                "fork point unavailable — the four-tree merge-safety classification cannot run \
                 (evidence: {evidence})"
            ));
            (BTreeMap::new(), Vec::new(), Vec::new())
        }
    };

    let classified = input.fork_base.is_some();
    let certified =
        classified && input.authority.source.certifiable() && invariant_violations.is_empty();
    let verdict = if !invariant_violations.is_empty() {
        "do_not_merge"
    } else if certified {
        "safe"
    } else {
        "uncertified"
    };
    let uncertified_cause = if verdict != "uncertified" {
        None
    } else if input.authority.mismatch.is_some() {
        Some("target_authority_mismatch")
    } else if !classified {
        Some("fork_base_unavailable")
    } else {
        Some("stale_local_target")
    };
    if let Some(mismatch) = input.authority.mismatch.as_ref() {
        reasons.push(format!(
            "target authority mismatch: {mismatch}; refusing to certify (evidence: {evidence})"
        ));
    }
    if uncertified_cause == Some("stale_local_target") {
        reasons.push(format!(
            "target head is local-only and not verified fresh; refusing to certify the merge \
             prediction against a possibly stale target — run `oak fetch` (or use `oak branch \
             review --remote`) for an authoritative check (evidence: {evidence})"
        ));
    }

    MergeSafetyAssessment {
        json: MergeSafetyJson {
            certified,
            verdict,
            uncertified_cause,
            target_head_source: input.authority.source.as_str(),
            assessed_target: input.assessed_target.to_string(),
            fetched_target: input.authority.fetched_target,
            fetched_target_head: input.authority.fetched_target_head,
            fork_base_commit: input.fork_base_commit.map(ToString::to_string),
            branch_head: input.branch_head.map(ToString::to_string),
            target_head: input.target_head.map(ToString::to_string),
            invariant_violation_count: invariant_violations.len(),
            invariant_violations_sample: sample(&invariant_violations),
            overlap_count: overlap_paths.len(),
            overlap_paths_sample: sample(&overlap_paths),
            reasons,
        },
        invariant_violations,
        per_file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{hash_string, ManifestEntry};

    fn entry(path: &str, content: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string(content),
            mode: FileMode::Regular,
        }
    }

    fn manifest(entries: Vec<ManifestEntry>) -> Manifest {
        Manifest::new(entries)
    }

    fn hash(byte: &str) -> Hash {
        Hash(byte.repeat(32))
    }

    fn authority(source: TargetHeadSource) -> TargetAuthority {
        TargetAuthority {
            source,
            fetched_target: None,
            fetched_target_head: None,
            mismatch: None,
        }
    }

    fn assess_with(
        fork_base: Option<&Manifest>,
        branch: &Manifest,
        target: &Manifest,
        predicted: &Manifest,
        conflicts: &HashSet<String>,
        source: TargetHeadSource,
    ) -> MergeSafetyAssessment {
        let fork = hash("aa");
        let branch_head = hash("bb");
        let target_head = hash("cc");
        assess(MergeSafetyInput {
            fork_base,
            branch,
            target,
            predicted,
            predicted_conflicts: conflicts,
            assessed_target: "main",
            authority: authority(source),
            fork_base_commit: fork_base.map(|_| &fork),
            branch_head: Some(&branch_head),
            target_head: Some(&target_head),
        })
    }

    #[test]
    fn fb105_shape_target_only_files_dropped_is_invariant_violation() {
        // The fb-105 / fb-7d2bbb shape: a stale branch forked before the
        // target added new files; the branch never touched them; a "clean"
        // predicted result deletes them anyway. Hard-fail.
        let fork_base = manifest(vec![entry("src/lib.rs", "v1")]);
        let branch = manifest(vec![entry("src/lib.rs", "v1-branch-edit")]);
        let target = manifest(vec![
            entry("src/lib.rs", "v1"),
            entry("src/new_module.rs", "added on main after fork"),
            entry("docs/runbook.md", "also added on main"),
        ]);
        // Buggy predicted result: carries the branch edit but silently drops
        // the current-main-only files.
        let predicted = manifest(vec![entry("src/lib.rs", "v1-branch-edit")]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert_eq!(
            assessment.invariant_violations,
            vec![
                "docs/runbook.md".to_string(),
                "src/new_module.rs".to_string()
            ],
        );
        assert_eq!(assessment.json.verdict, "do_not_merge");
        assert_eq!(assessment.json.invariant_violation_count, 2);
        assert_eq!(
            assessment.json.invariant_violations_sample,
            assessment.invariant_violations
        );
        assert!(!assessment.json.certified);
        assert_eq!(
            assessment.per_file.get("src/new_module.rs"),
            Some(&MergeSafetyClass::InvariantViolation)
        );
        assert_eq!(
            assessment.per_file.get("src/lib.rs"),
            Some(&MergeSafetyClass::BranchChange)
        );
        // Reasons name each file and the tree evidence.
        let violation_reasons: Vec<&String> = assessment
            .json
            .reasons
            .iter()
            .filter(|reason| reason.contains("merge invariant violation"))
            .collect();
        assert_eq!(violation_reasons.len(), 2);
        assert!(violation_reasons
            .iter()
            .any(|reason| reason.contains("src/new_module.rs") && reason.contains("deletes it")));
        assert!(violation_reasons
            .iter()
            .all(|reason| reason.contains("fork_base=") && reason.contains("target_head=")));
    }

    #[test]
    fn target_only_change_reverted_is_invariant_violation() {
        // Target modified a file after the fork; branch never touched it;
        // predicted result reverts it to the fork-base content.
        let fork_base = manifest(vec![entry("config.toml", "old")]);
        let branch = manifest(vec![entry("config.toml", "old"), entry("feat.rs", "new")]);
        let target = manifest(vec![entry("config.toml", "updated on main")]);
        let predicted = manifest(vec![entry("config.toml", "old"), entry("feat.rs", "new")]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert_eq!(
            assessment.invariant_violations,
            vec!["config.toml".to_string()]
        );
        assert_eq!(assessment.json.verdict, "do_not_merge");
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("config.toml") && reason.contains("reverts or alters")));
    }

    #[test]
    fn intentional_branch_deletion_is_clean() {
        // Branch deleted a file the target has not touched since the fork:
        // a valid contribution, no warning.
        let fork_base = manifest(vec![entry("kept.rs", "v1"), entry("removed.rs", "v1")]);
        let branch = manifest(vec![entry("kept.rs", "v1")]);
        let target = manifest(vec![
            entry("kept.rs", "v1"),
            entry("removed.rs", "v1"),
            entry("new_on_main.rs", "v1"),
        ]);
        // Correct predicted result: deletion carried, main-only file kept.
        let predicted = manifest(vec![entry("kept.rs", "v1"), entry("new_on_main.rs", "v1")]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert!(assessment.invariant_violations.is_empty());
        assert_eq!(assessment.json.verdict, "safe");
        assert!(assessment.json.certified);
        assert_eq!(assessment.json.uncertified_cause, None);
        assert_eq!(
            assessment.per_file.get("removed.rs"),
            Some(&MergeSafetyClass::BranchChange)
        );
        assert!(!assessment.per_file.contains_key("new_on_main.rs"));
    }

    #[test]
    fn both_sides_changed_is_overlap_warning_not_violation() {
        let fork_base = manifest(vec![entry("shared.rs", "base")]);
        let branch = manifest(vec![entry("shared.rs", "branch edit")]);
        let target = manifest(vec![entry("shared.rs", "target edit")]);
        let predicted = manifest(vec![entry("shared.rs", "merged content")]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert!(assessment.invariant_violations.is_empty());
        assert_eq!(assessment.json.overlap_count, 1);
        assert_eq!(
            assessment.json.overlap_paths_sample,
            vec!["shared.rs".to_string()]
        );
        assert_eq!(assessment.json.verdict, "safe");
        assert_eq!(
            assessment.per_file.get("shared.rs"),
            Some(&MergeSafetyClass::Overlap)
        );
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("shared.rs") && reason.contains("both sides")));
    }

    #[test]
    fn predicted_conflicts_are_excluded_from_classification() {
        let fork_base = manifest(vec![entry("clash.rs", "base")]);
        let branch = manifest(vec![entry("clash.rs", "branch")]);
        let target = manifest(vec![entry("clash.rs", "target")]);
        let predicted = manifest(vec![]);
        let conflicts: HashSet<String> = ["clash.rs".to_string()].into_iter().collect();

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &conflicts,
            TargetHeadSource::LiveRemoteFetch,
        );
        assert!(assessment.per_file.is_empty());
        assert!(assessment.invariant_violations.is_empty());
        assert_eq!(assessment.json.verdict, "safe");
    }

    #[test]
    fn stale_local_target_refuses_to_certify() {
        let fork_base = manifest(vec![entry("a.rs", "v1")]);
        let branch = manifest(vec![entry("a.rs", "v2")]);
        let target = manifest(vec![entry("a.rs", "v1")]);
        let predicted = manifest(vec![entry("a.rs", "v2")]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::StaleLocal,
        );
        assert!(assessment.invariant_violations.is_empty());
        assert!(!assessment.json.certified);
        assert_eq!(assessment.json.verdict, "uncertified");
        assert_eq!(
            assessment.json.uncertified_cause,
            Some("stale_local_target")
        );
        assert_eq!(assessment.json.target_head_source, "stale_local");
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("refusing to certify")));
    }

    #[test]
    fn missing_fork_base_refuses_to_certify() {
        let branch = manifest(vec![entry("a.rs", "v2")]);
        let target = manifest(vec![entry("a.rs", "v1")]);
        let predicted = manifest(vec![entry("a.rs", "v2")]);

        let assessment = assess_with(
            None,
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert!(!assessment.json.certified);
        assert_eq!(assessment.json.verdict, "uncertified");
        assert_eq!(
            assessment.json.uncertified_cause,
            Some("fork_base_unavailable")
        );
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("fork point unavailable")));
    }

    #[test]
    fn violations_block_even_against_stale_target_data() {
        // A violation computed against even a stale target is still damning:
        // the stale target already shows destroyed paths.
        let fork_base = manifest(vec![]);
        let branch = manifest(vec![]);
        let target = manifest(vec![entry("main_only.rs", "v1")]);
        let predicted = manifest(vec![]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::StaleLocal,
        );
        assert_eq!(
            assessment.invariant_violations,
            vec!["main_only.rs".to_string()]
        );
        assert_eq!(assessment.json.verdict, "do_not_merge");
        // do_not_merge, not uncertified: the violation is the verdict.
        assert_eq!(assessment.json.uncertified_cause, None);
    }

    #[test]
    fn mode_only_target_change_dropped_is_violation() {
        // chmod on the target side since the fork, branch untouched,
        // predicted result loses the mode change.
        let mut base_entry = entry("tool.sh", "same");
        base_entry.mode = FileMode::Regular;
        let mut target_entry = entry("tool.sh", "same");
        target_entry.mode = FileMode::Executable;

        let fork_base = manifest(vec![base_entry.clone()]);
        let branch = manifest(vec![base_entry.clone()]);
        let target = manifest(vec![target_entry]);
        let predicted = manifest(vec![base_entry]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert_eq!(assessment.invariant_violations, vec!["tool.sh".to_string()]);
    }

    #[test]
    fn live_fetch_of_matching_target_and_head_certifies() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let head = hash("cc");
        let auth = resolve_target_authority(
            &repo,
            &TargetDataOrigin::LiveFetched {
                target: "main",
                head: Some(&head),
            },
            "main",
            &head,
        )
        .unwrap();
        assert_eq!(auth.source, TargetHeadSource::LiveRemoteFetch);
        assert!(auth.mismatch.is_none());
        assert_eq!(auth.fetched_target.as_deref(), Some("main"));
        assert_eq!(auth.fetched_target_head, Some(head.to_string()));
    }

    #[test]
    fn live_fetch_of_different_target_is_authority_mismatch() {
        // The caller fetched --against some other branch while the
        // assessment classified against the branch's recorded parent: the
        // fetch's authority must not transfer.
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let fetched_head = hash("dd");
        let assessed_head = hash("cc");
        let auth = resolve_target_authority(
            &repo,
            &TargetDataOrigin::LiveFetched {
                target: "release-branch",
                head: Some(&fetched_head),
            },
            "main",
            &assessed_head,
        )
        .unwrap();
        assert_eq!(auth.source, TargetHeadSource::LiveFetchMismatch);
        let mismatch = auth.mismatch.clone().unwrap();
        assert!(mismatch.contains("release-branch") && mismatch.contains("main"));

        // And the assessment reports uncertified with the mismatch cause.
        let fork_base = manifest(vec![entry("a.rs", "v1")]);
        let branch = manifest(vec![entry("a.rs", "v2")]);
        let target = manifest(vec![entry("a.rs", "v1")]);
        let predicted = manifest(vec![entry("a.rs", "v2")]);
        let assessment = assess(MergeSafetyInput {
            fork_base: Some(&fork_base),
            branch: &branch,
            target: &target,
            predicted: &predicted,
            predicted_conflicts: &HashSet::new(),
            assessed_target: "main",
            authority: auth,
            fork_base_commit: None,
            branch_head: None,
            target_head: Some(&assessed_head),
        });
        assert!(!assessment.json.certified);
        assert_eq!(assessment.json.verdict, "uncertified");
        assert_eq!(
            assessment.json.uncertified_cause,
            Some("target_authority_mismatch")
        );
        assert_eq!(assessment.json.target_head_source, "live_fetch_mismatch");
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("target authority mismatch")));
    }

    #[test]
    fn same_target_different_head_is_authority_mismatch() {
        // Fetched main, but the assessed head is not the fetched head (e.g.
        // main moved between fetch and assessment): no certification.
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let fetched_head = hash("dd");
        let assessed_head = hash("cc");
        let auth = resolve_target_authority(
            &repo,
            &TargetDataOrigin::LiveFetched {
                target: "main",
                head: Some(&fetched_head),
            },
            "main",
            &assessed_head,
        )
        .unwrap();
        assert_eq!(auth.source, TargetHeadSource::LiveFetchMismatch);
    }

    #[test]
    fn violation_output_is_bounded_to_sample_plus_count() {
        // 8 target-only files dropped: the canonical top-level list is
        // uncapped locally and keeps all 8, while the JSON block and reasons
        // carry a bounded sample plus the count.
        let fork_base = manifest(vec![]);
        let branch = manifest(vec![]);
        let entries: Vec<ManifestEntry> = (0..8)
            .map(|i| entry(&format!("main_only_{i}.rs"), "v1"))
            .collect();
        let target = manifest(entries);
        let predicted = manifest(vec![]);

        let assessment = assess_with(
            Some(&fork_base),
            &branch,
            &target,
            &predicted,
            &HashSet::new(),
            TargetHeadSource::LiveRemoteFetch,
        );
        assert_eq!(assessment.invariant_violations.len(), 8);
        assert_eq!(assessment.json.invariant_violation_count, 8);
        assert_eq!(
            assessment.json.invariant_violations_sample.len(),
            PATH_SAMPLE_LIMIT
        );
        let per_file_reasons = assessment
            .json
            .reasons
            .iter()
            .filter(|reason| reason.contains("merge invariant violation"))
            .count();
        assert_eq!(per_file_reasons, PATH_SAMPLE_LIMIT);
        assert!(assessment
            .json
            .reasons
            .iter()
            .any(|reason| reason.contains("and 3 more invariant violation(s)")));
        assert!(bounded_path_list(&assessment.invariant_violations).contains("… and 3 more"));
    }

    #[test]
    fn classifier_is_linear_on_large_manifests() {
        // Perf-shaped sanity check: ~100k paths per tree. The classifier
        // indexes each manifest once (O(N)); the old shape (Manifest::get
        // per path = linear scan) was O(N^2) — ~10^10 signature comparisons
        // here, which would blow far past this generous bound.
        let n = 100_000usize;
        let entries: Vec<ManifestEntry> = (0..n)
            .map(|i| entry(&format!("src/file_{i:06}.rs"), "v1"))
            .collect();
        let fork_base = manifest(entries.clone());
        let branch = manifest(entries.clone());
        let mut target_entries = entries;
        target_entries.push(entry("src/target_only.rs", "added on main"));
        let target = manifest(target_entries);
        // Predicted result drops the target-only file: one violation among
        // 100k clean paths.
        let predicted = manifest(
            (0..n)
                .map(|i| entry(&format!("src/file_{i:06}.rs"), "v1"))
                .collect(),
        );

        let started = std::time::Instant::now();
        let classes =
            classify_predicted_merge(&fork_base, &branch, &target, &predicted, &HashSet::new());
        let elapsed = started.elapsed();
        assert_eq!(
            classes.get("src/target_only.rs"),
            Some(&MergeSafetyClass::InvariantViolation)
        );
        assert_eq!(classes.len(), 1);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "classifier took {elapsed:?}; expected a linear pass (quadratic would take minutes)"
        );
    }
}
