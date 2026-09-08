//! Read-only cumulative prediction for pinned, independent sibling branches.
use super::{predict_conflicts_with, RemoteAnalysisBranch};
use crate::commands::{blob_fetch, branch, merge};
use oak_core::{
    Commit, Hash, Manifest, MetadataKey, OakError, Repository, Result, SqliteRepository,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

const MAX_BRANCHES: usize = 32;
const MAX_COMMITS: usize = 10_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_METADATA: usize = 1024 * 1024;

#[derive(Serialize)]
struct Pin {
    branch: String,
    head: Hash,
    parent: Option<String>,
}

#[derive(Serialize)]
struct Step {
    source: Pin,
    fork: Option<Hash>,
    fork_kind: &'static str,
    input_tree: Hash,
    candidate_tree: Option<Hash>,
    state: &'static str,
    conflict_files: Vec<super::ConflictFileJson>,
    validation: &'static str,
}

#[derive(Serialize)]
struct Preview {
    schema_version: u32,
    kind: &'static str,
    repository: serde_json::Value,
    observed_at: String,
    observation: &'static str,
    against: Pin,
    ordered_sources: Vec<Pin>,
    initial_tree: Option<Hash>,
    candidate_tree: Option<Hash>,
    state: &'static str,
    reason: Option<&'static str>,
    steps: Vec<Step>,
    remote_recheck: &'static str,
    candidate_storage: &'static str,
    limits: serde_json::Value,
    validation: &'static str,
    recommended_next_commands: Vec<&'static str>,
}

fn incomplete(reason: &'static str) -> OakError {
    OakError::Server(format!("Train preview incomplete: {reason}"))
}

fn pin(branch: &RemoteAnalysisBranch) -> Result<Pin> {
    Ok(Pin {
        branch: branch.name.clone(),
        head: branch
            .head
            .clone()
            .ok_or_else(|| incomplete("missing branch head"))?,
        parent: branch.parent.clone(),
    })
}

fn local_pin(repo: &dyn Repository, name: &str) -> Result<RemoteAnalysisBranch> {
    let branch = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.into()))?;
    Ok(RemoteAnalysisBranch {
        name: name.into(),
        head: repo.get_branch_head(name)?,
        parent: branch.parent_branch,
    })
}

#[derive(Deserialize)]
struct BranchList {
    branches: Vec<branch::RemoteBranchData>,
}

// Unlike legacy branch listing, train metadata shares a deadline and byte cap.
// Redirects and malformed diagnostics never expose bodies, URLs, or credentials.
async fn remote_pins(
    remote: &branch::RemoteIdentity,
    names: &[String],
    budget: &blob_fetch::ReviewPreparationBudget,
) -> Result<Vec<RemoteAnalysisBranch>> {
    let request = async {
        let mut url =
            reqwest::Url::parse(&remote.remote_url).map_err(|_| incomplete("invalid remote"))?;
        url.set_query(None);
        url.set_fragment(None);
        url.set_username("")
            .map_err(|_| incomplete("invalid remote"))?;
        url.set_password(None)
            .map_err(|_| incomplete("invalid remote"))?;
        url.path_segments_mut()
            .map_err(|_| incomplete("invalid remote"))?
            .pop_if_empty()
            .push("api")
            .push(&remote.owner)
            .push(&remote.repo_name)
            .push("branches");
        let mut request = crate::http::api_client()
            .get(url)
            .timeout(budget.remaining());
        if let Some(token) = remote.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| incomplete("metadata transport failed"))?;
        if !response.status().is_success() {
            return Err(incomplete(
                "metadata request rejected or redirected; response omitted",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_METADATA as u64)
        {
            return Err(incomplete("metadata byte budget exceeded"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| incomplete("metadata stream failed"))?
        {
            if chunk.len() > MAX_METADATA.saturating_sub(bytes.len()) {
                return Err(incomplete("metadata byte budget exceeded"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let list: BranchList = serde_json::from_slice(&bytes)
            .map_err(|_| incomplete("malformed metadata; diagnostics omitted"))?;
        names
            .iter()
            .map(|name| {
                let mut matches = list.branches.iter().filter(|branch| &branch.name == name);
                let branch = matches
                    .next()
                    .ok_or_else(|| incomplete("requested remote branch missing"))?;
                if matches.next().is_some() {
                    return Err(incomplete("duplicate remote branch identity"));
                }
                Ok(RemoteAnalysisBranch {
                    name: name.clone(),
                    head: branch
                        .head
                        .as_deref()
                        .map(Hash::from_hex)
                        .transpose()
                        .map_err(|_| incomplete("malformed remote head"))?,
                    parent: branch.parent_branch.clone(),
                })
            })
            .collect()
    };
    tokio::time::timeout(budget.remaining(), request)
        .await
        .map_err(|_| incomplete("metadata deadline exceeded"))?
}

struct Calculation<'a> {
    commits: HashMap<Hash, Commit>,
    target_history_incomplete: bool,
    blobs: HashMap<Hash, Vec<u8>>,
    bytes_left: usize,
    budget: &'a mut blob_fetch::ReviewPreparationBudget,
}

impl<'a> Calculation<'a> {
    fn new(budget: &'a mut blob_fetch::ReviewPreparationBudget) -> Self {
        Self {
            commits: HashMap::new(),
            target_history_incomplete: false,
            blobs: HashMap::new(),
            bytes_left: MAX_BYTES,
            budget,
        }
    }

    // Target membership is positive evidence only: missing older target rows
    // are NOT boundaries. Every source-exclusive edge must reach a verified
    // target commit or an actual root. The separate merge-base resolver still
    // rejects missing target edges that could hide a nearer common ancestor.
    // Hash/decoding errors (including tampered cycles) are never skipped.
    fn ancestry(
        &mut self,
        repo: &dyn Repository,
        head: &Hash,
        target_boundaries: Option<&HashSet<Hash>>,
    ) -> Result<HashSet<Hash>> {
        let mut seen = HashSet::new();
        let mut verified_hashes = HashSet::new();
        let mut queue = VecDeque::from([head.clone()]);
        while let Some(hash) = queue.pop_front() {
            self.budget.check_deadline()?;
            if target_boundaries.is_some_and(|boundaries| boundaries.contains(&hash))
                || !seen.insert(hash.clone())
            {
                continue;
            }
            if !self.commits.contains_key(&hash) {
                if self.commits.len() >= MAX_COMMITS {
                    return Err(incomplete("ancestry work budget exceeded"));
                }
                let Some(c) = repo.get_commit(&hash)? else {
                    if target_boundaries.is_none() && &hash != head {
                        self.target_history_incomplete = true;
                        continue;
                    }
                    return Err(OakError::CommitNotFound(hash.to_string()));
                };
                let verified = Commit::rehydrate_verified(
                    &hash,
                    c.branch_name,
                    c.parent_hash,
                    c.merge_parent_hash,
                    c.manifest_hash,
                    c.author,
                    c.message,
                    c.files,
                    c.timestamp,
                )?;
                self.commits.insert(hash.clone(), verified);
            }
            verified_hashes.insert(hash.clone());
            let c = &self.commits[&hash];
            queue.extend(
                c.parent_hash
                    .iter()
                    .chain(c.merge_parent_hash.iter())
                    .cloned(),
            );
        }
        Ok(verified_hashes)
    }

    fn manifest(&mut self, repo: &SqliteRepository, hash: &Hash) -> Result<Manifest> {
        let c = self
            .commits
            .get(hash)
            .ok_or_else(|| OakError::CommitNotFound(hash.to_string()))?;
        self.budget
            .inspect(&c.manifest_hash, |hash| repo.get_tree(hash))?;
        let manifest = if c.manifest_hash == Manifest::empty().hash {
            Manifest::empty()
        } else {
            repo.get_manifest(&c.manifest_hash)?
                .ok_or_else(|| OakError::ManifestNotFound(c.manifest_hash.to_string()))?
        };
        if oak_core::tree::build_tree(&manifest.entries)?.root_hash != c.manifest_hash {
            return Err(OakError::InvalidHash("train snapshot tree mismatch".into()));
        }
        for entry in &manifest.entries {
            self.budget.check_deadline()?;
            if self.blobs.contains_key(&entry.blob_hash) {
                continue;
            }
            let bytes = if entry.blob_hash == oak_core::hash_bytes(&[]) {
                Vec::new()
            } else {
                let size = repo
                    .get_blob_size(&entry.blob_hash)?
                    .ok_or_else(|| OakError::BlobNotFound(entry.blob_hash.to_string()))?;
                if size > self.bytes_left as u64 {
                    return Err(incomplete("logical blob byte budget exceeded"));
                }
                // Verify bounded decoding before materializing from the same
                // pinned read transaction; stored size alone is not proof.
                let evidence = repo.inspect_pinned_file(
                    hash.as_str(),
                    &entry.path,
                    self.bytes_left.max(1) as u64,
                )?;
                use oak_core::sqlite::file_inspect::InspectionStatus;
                match evidence.status {
                    InspectionStatus::Verified => {}
                    InspectionStatus::BudgetExceeded => {
                        return Err(incomplete("logical blob budget exceeded"))
                    }
                    InspectionStatus::ObjectMissing => {
                        return Err(OakError::BlobNotFound(entry.blob_hash.to_string()))
                    }
                    InspectionStatus::Corrupt => {
                        return Err(OakError::InvalidHash(
                            "train file verification failed".into(),
                        ))
                    }
                    _ => return Err(incomplete("bounded file verification unavailable")),
                }
                repo.get_blob(&entry.blob_hash)?
                    .ok_or_else(|| OakError::BlobNotFound(entry.blob_hash.to_string()))?
                    .content
            };
            if bytes.len() > self.bytes_left {
                return Err(incomplete("logical blob byte budget exceeded"));
            }
            if oak_core::hash_bytes(&bytes) != entry.blob_hash {
                return Err(OakError::InvalidHash("train blob mismatch".into()));
            }
            self.bytes_left -= bytes.len();
            self.blobs.insert(entry.blob_hash.clone(), bytes);
        }
        Ok(manifest)
    }
}

fn compute(
    repo: &SqliteRepository,
    sources: &[RemoteAnalysisBranch],
    target: &RemoteAnalysisBranch,
    preview: &mut Preview,
    budget: &mut blob_fetch::ReviewPreparationBudget,
) -> Result<()> {
    let mut calculation = Calculation::new(budget);
    let target_head = &pin(target)?.head;
    let target_ancestry = calculation.ancestry(repo, target_head, None)?;
    let mut contributions = HashSet::new();
    for source in sources {
        let exclusive = calculation.ancestry(repo, &pin(source)?.head, Some(&target_ancestry))?;
        for hash in exclusive {
            if !contributions.insert(hash) {
                preview.state = "unsupported";
                preview.reason = Some("dependent_or_shared_unlanded_ancestry");
                return Ok(());
            }
        }
    }
    let mut candidate = calculation.manifest(repo, target_head)?;
    preview.initial_tree = Some(candidate.hash.clone());
    for source in sources {
        calculation.budget.check_deadline()?;
        let source_pin = pin(source)?;
        let mut step = Step {
            source: source_pin,
            fork: None,
            fork_kind: "unavailable",
            input_tree: candidate.hash.clone(),
            candidate_tree: None,
            state: "incomplete",
            conflict_files: vec![],
            validation: "required_for_candidate; no_CI_or_publication_authority",
        };
        let result = (|| -> Result<Manifest> {
            let base = match merge::resolve_merge_base_identity_from_heads(
                repo,
                &source.name,
                source.head.as_ref(),
                source.parent.as_deref(),
                &target.name,
                target.head.as_ref(),
            )? {
                merge::MergeBaseIdentity::Commit(hash) => {
                    step.fork = Some(hash.clone());
                    step.fork_kind = "commit";
                    calculation.manifest(repo, &hash)?
                }
                merge::MergeBaseIdentity::DeclaredRoot => {
                    // The legacy declared-root fallback precedes its missing
                    // target check. Unknown target history could contain this
                    // source root; train cannot prove an empty base there.
                    if calculation.target_history_incomplete {
                        return Err(incomplete("merge base unavailable"));
                    }
                    step.fork_kind = "declared_root";
                    Manifest::empty()
                }
                merge::MergeBaseIdentity::Unavailable(_) => {
                    return Err(incomplete("merge base unavailable"))
                }
            };
            let source_manifest = calculation.manifest(repo, &step.source.head)?;
            let outcome = oak_core::three_way_merge_manifests(&base, &source_manifest, &candidate);
            // The legacy helper tolerates a non-text base by treating it as
            // empty. Train evidence cannot certify that fallback: overlapping
            // binary content requires explicit resolution, including a binary
            // base replaced by text on both sides.
            if outcome.conflicts.iter().any(|conflict| {
                conflict
                    .branch_entry
                    .iter()
                    .chain(conflict.parent_entry.iter())
                    .chain(base.get(&conflict.path))
                    .any(|entry| {
                        calculation
                            .blobs
                            .get(&entry.blob_hash)
                            .is_some_and(|bytes| {
                                bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
                            })
                    })
            }) {
                step.state = "conflict";
                step.conflict_files = outcome
                    .conflicts
                    .iter()
                    .map(|conflict| super::ConflictFileJson {
                        path: conflict.path.clone(),
                        conflict_type: "binary_or_unresolved",
                    })
                    .collect();
                step.conflict_files
                    .sort_by(|left, right| left.path.cmp(&right.path));
                return Ok(candidate.clone());
            }
            let prediction = predict_conflicts_with(
                &base,
                outcome.conflicts,
                &source.name,
                &target.name,
                |hash| {
                    calculation
                        .blobs
                        .get(hash)
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| OakError::BlobNotFound(hash.to_string()))
                },
            )?;
            step.conflict_files = prediction.conflict_files;
            if !step.conflict_files.is_empty() {
                step.state = "conflict";
                return Ok(candidate.clone());
            }
            for (hash, bytes) in prediction.merged_blobs {
                if !calculation.blobs.contains_key(&hash) {
                    if bytes.len() > calculation.bytes_left {
                        return Err(incomplete("candidate blob byte budget exceeded"));
                    }
                    calculation.bytes_left -= bytes.len();
                    calculation.blobs.insert(hash, bytes);
                }
            }
            let mut entries = outcome.clean_entries;
            entries.extend(prediction.resolved_entries);
            oak_core::tree::build_tree(&entries)?;
            let merged = Manifest::new(entries);
            step.state = "predicted";
            step.candidate_tree = Some(merged.hash.clone());
            Ok(merged)
        })();
        let stopped = step.state != "predicted";
        preview.steps.push(step);
        candidate = result?;
        if stopped {
            preview.state = "conflict";
            return Ok(());
        }
    }
    preview.candidate_tree = Some(candidate.hash);
    preview.state = "predicted";
    Ok(())
}

fn error_reason(error: &OakError) -> &'static str {
    match error {
        OakError::BlobNotFound(_) | OakError::IncompleteBlobData { .. } => "missing_blob",
        OakError::CommitNotFound(_) | OakError::IncompleteAncestry { .. } => "missing_ancestry",
        OakError::ManifestNotFound(_) | OakError::IncompleteManifestData { .. } => "missing_tree",
        OakError::InvalidHash(_) => "corrupt_object",
        OakError::Server(message) if message.contains("budget") || message.contains("deadline") => {
            "budget_exceeded"
        }
        OakError::Server(message) if message.contains("merge base unavailable") => {
            "missing_merge_base"
        }
        _ => "unavailable; diagnostics_omitted",
    }
}

pub async fn run(
    path: &Path,
    names: &[String],
    against: &str,
    remote: bool,
    json: bool,
) -> Result<bool> {
    if names.is_empty()
        || names.len() > MAX_BRANCHES
        || names.iter().collect::<HashSet<_>>().len() != names.len()
        || names.iter().any(|name| name == against)
    {
        return Err(OakError::InvalidArgument(
            "Train requires 1–32 unique source branches distinct from --against".into(),
        ));
    }
    for name in names.iter().map(String::as_str).chain([against]) {
        if name.len() > 255 {
            return Err(OakError::InvalidArgument(
                "Train branch names are limited to 255 bytes".into(),
            ));
        }
        if name != oak_core::DEFAULT_BRANCH {
            branch::validate_branch_name(name)?;
        }
    }
    let ctx = crate::resolve::resolve(path)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(incomplete("train requires an SQLite checkout"));
    }
    let local = SqliteRepository::open_read_only(&ctx.db_path()?)?;
    if oak_core::HashFormat::from_metadata(local.get_metadata(MetadataKey::HashFormat)?.as_deref())?
        != oak_core::HashFormat::V1
    {
        return Err(incomplete("unsupported object format"));
    }
    let all_names: Vec<String> = std::iter::once(against.to_owned())
        .chain(names.iter().cloned())
        .collect();
    let mut budget = blob_fetch::ReviewPreparationBudget::new();
    let workspace = if remote {
        Some(branch::RemoteWorkspace::open(path)?)
    } else {
        None
    };
    let pins: Vec<_> = if let Some(workspace) = &workspace {
        remote_pins(&workspace.remote, &all_names, &budget).await?
    } else {
        all_names
            .iter()
            .map(|name| local_pin(&local, name))
            .collect::<Result<_>>()?
    };
    let target = &pins[0];
    let sources = &pins[1..];
    let origin = local
        .get_metadata(MetadataKey::RemoteUrl)?
        .and_then(|url| reqwest::Url::parse(&url).ok())
        .map(|url| url.origin().ascii_serialization());
    let mut preview = Preview {
        schema_version: 1,
        kind: "branch_train_preview",
        repository: serde_json::json!({"origin":origin,"owner":local.get_metadata(MetadataKey::RepoOwner)?,"name":local.get_metadata(MetadataKey::RepoName)?}),
        observed_at: chrono::Utc::now().to_rfc3339(),
        observation: if remote { "remote_branch_list" } else { "local_read_snapshot" },
        against: pin(target)?,
        ordered_sources: sources.iter().map(pin).collect::<Result<_>>()?,
        initial_tree: None,
        candidate_tree: None,
        state: "incomplete",
        reason: None,
        steps: vec![],
        remote_recheck: "not_requested",
        candidate_storage: "memory_only; no_synthetic_commits_or_objects_stored",
        limits: serde_json::json!({"branches":MAX_BRANCHES,"unique_ancestry_commits":MAX_COMMITS,"logical_blob_bytes":MAX_BYTES,"remote_metadata_bytes_per_response":MAX_METADATA,"deadline_seconds":30}),
        validation: "prediction_only; validate_each_candidate; not_semantic_equivalence_or_landing_authority",
        recommended_next_commands: vec!["oak branch train --help"],
    };
    if let Some(workspace) = &workspace {
        let heads: Vec<_> = pins
            .iter()
            .filter_map(|branch| branch.head.clone())
            .collect();
        let prepared = async {
            blob_fetch::ensure_review_commits_with_budget(
                &workspace.repo,
                &workspace.remote.remote_url,
                &workspace.remote.owner,
                &workspace.remote.repo_name,
                workspace.remote.token.as_deref(),
                &heads,
                &mut budget,
            )
            .await?;
            // Bound and verify available target history and all exclusive
            // source edges before the existing exact base resolver traverses
            // them. Missing target rows are never positive membership proof.
            {
                let mut verification = Calculation::new(&mut budget);
                let target_ancestry = verification.ancestry(&workspace.repo, &heads[0], None)?;
                for head in &heads[1..] {
                    verification.ancestry(&workspace.repo, head, Some(&target_ancestry))?;
                }
            }
            super::prepare_remote_review_bases_from_snapshots(
                &workspace.repo,
                &workspace.remote,
                sources,
                target,
                &mut budget,
            )
            .await
        }
        .await;
        match prepared.and_then(|_| {
            let snapshot = SqliteRepository::open_read_only(&ctx.db_path()?)?;
            compute(&snapshot, sources, target, &mut preview, &mut budget)
        }) {
            Ok(()) => {}
            Err(error) => {
                preview.state = "incomplete";
                preview.reason = Some(error_reason(&error));
            }
        }
        match remote_pins(&workspace.remote, &all_names, &budget).await {
            Ok(observed)
                if observed
                    .iter()
                    .zip(&pins)
                    .all(|(new, old)| new.head == old.head && new.parent == old.parent) =>
            {
                preview.remote_recheck = "unchanged_at_final_observation_only"
            }
            Ok(_) => {
                preview.remote_recheck = "moved";
                preview.state = "stale";
                preview.reason = Some("remote_source_or_target_moved");
                preview.candidate_tree = None;
            }
            Err(_) => {
                preview.remote_recheck = "unavailable";
                preview.state = "incomplete";
                preview.reason = Some("final_remote_observation_unavailable");
                preview.candidate_tree = None;
            }
        }
    } else if let Err(error) = compute(&local, sources, target, &mut preview, &mut budget) {
        preview.state = "incomplete";
        preview.reason = Some(error_reason(&error));
    }
    if json {
        crate::output::print_json(&preview)?;
    } else {
        crate::output::print_line(&format!(
            "Train preview: {} ({} of {} steps predicted)",
            preview.state,
            preview
                .steps
                .iter()
                .filter(|step| step.state == "predicted")
                .count(),
            names.len()
        ));
        for step in &preview.steps {
            crate::output::print_line(&format!(
                "{} {}: {}",
                step.source.branch, step.source.head, step.state
            ));
        }
        if let Some(reason) = preview.reason {
            crate::output::print_line(reason);
        }
        crate::output::print_line(
            "Prediction only; every cumulative candidate still requires validation.",
        );
    }
    Ok(preview.state == "predicted")
}
