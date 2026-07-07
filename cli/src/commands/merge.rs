use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use oak_core::{
    three_way_merge_manifests, BranchStatus, Commit, FileChange, Hash, IgnorePatterns, Manifest,
    ManifestEntry, MergeConflict, MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};

use crate::materialize::{apply_manifest, ApplyOpts, DeleteScope};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Merge the current branch into its parent branch.
///
/// When the parent is `main` the merge can't happen locally — `main` only
/// exists on the server, and the canonical merge is a squash-merge whose
/// commit message is the branch's description (see CLAUDE.md). In that
/// case we call the server's
/// `POST /api/:owner/:name/branches/:brname/merge` endpoint and
/// mirror the resulting closed state into the local repo.
///
/// The remote comes from the repo's stored `RemoteUrl`; when that host
/// answers with a redirect to a trusted Oak host, the stored remote is
/// updated and the merge retried once — see `follow_remote_move`. The
/// first attempt fails on its first request (the pre-merge push's HEAD
/// GET), before anything is mutated, so the retry re-runs the whole flow
/// safely.
pub async fn run(
    path: &Path,
    continue_merge: bool,
    abort_merge: bool,
    branch: Option<&str>,
) -> Result<()> {
    match run_once(path, continue_merge, abort_merge, branch).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            let old = super::stored_remote_url(path)?.unwrap_or_default();
            super::follow_remote_move(path, &old, &origin)?;
            run_once(path, continue_merge, abort_merge, branch).await
        }
        result => result,
    }
}

async fn run_once(
    path: &Path,
    continue_merge: bool,
    abort_merge: bool,
    branch: Option<&str>,
) -> Result<()> {
    if branch.is_some() && (continue_merge || abort_merge) {
        return Err(OakError::InvalidArgument(
            "`oak merge <branch>` cannot be combined with --continue or --abort".to_string(),
        ));
    }
    if continue_merge {
        return merge_continue(path);
    }
    if abort_merge {
        return merge_abort(path);
    }

    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;

    // Check if a merge is already in progress. The merge state files live in
    // the backend's sidecar dir (`.oak/` for sqlite, `.git/oak/` for git).
    if ctx.oak_dir.join("MERGE_HEAD").exists() {
        return Err(OakError::MergeInProgress);
    }

    // Refuse to merge over uncommitted work. Both merge paths end in a
    // whole-tree reset (to the merged manifest locally, or to main's new
    // head after a server squash-merge), which would silently wipe any
    // uncommitted edits.
    let (changes, _, _) = super::commit::get_status(path)?;
    if !changes.is_empty() {
        return Err(OakError::DirtyWorkingTree(
            "working tree has uncommitted changes — run `oak commit` to keep them \
             (or `oak reset` to discard them) before merging"
                .to_string(),
        ));
    }

    let repo = ctx.open()?;

    let current_branch_name = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    let branch_name = branch.unwrap_or(&current_branch_name).to_string();
    let explicit_branch = branch.is_some();

    // Get the branch and verify it has a parent
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;

    let parent_name = branch.parent_branch.ok_or_else(|| {
        OakError::BranchNotFound(format!(
            "branch '{branch_name}' has no parent to merge into"
        ))
    })?;

    // `main` only exists on the server. Landing a branch onto main goes
    // through the server's squash-merge endpoint, where the branch's
    // description becomes the (single) commit message.
    if parent_name == "main" {
        // Push the branch first: the server merges *its* copy of the branch,
        // so `oak commit && oak merge` without a push used to squash-merge a
        // stale server-side tip and then reset the worktree over the
        // unpushed commits. A successful push leaves the server's branch
        // head equal to the local one (the server CAS-checks the expected
        // head); any push failure aborts the merge. The git backend can't
        // push (sqlite-only), so it keeps the old behavior.
        if matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
            push_branch_for_merge(&ctx, &branch_name).await?;
        }
        return merge_to_main_via_server(&lock, repo.as_ref(), root, &branch_name).await;
    }

    if explicit_branch {
        return Err(OakError::InvalidArgument(format!(
            "`oak merge {branch_name}` is currently supported only for branches parented to main; run `oak switch {branch_name} --clean` then `oak merge` for local-parent merges"
        )));
    }

    // Get the current branch's head commit
    let branch_head = repo
        .get_branch_head(&branch_name)?
        .ok_or(OakError::NoCommits)?;

    let branch_commit =
        repo.get_commit(&branch_head)?
            .ok_or_else(|| OakError::IncompleteCommitData {
                context: format!("branch head for '{branch_name}'"),
                missing: branch_head.short().to_string(),
            })?;

    // Get the parent branch's head commit (if any)
    let parent_head = repo.get_branch_head(&parent_name)?;

    let parent_commit = if let Some(ref ph) = parent_head {
        Some(
            repo.get_commit(ph)?
                .ok_or_else(|| OakError::IncompleteCommitData {
                    context: format!("parent head for '{parent_name}'"),
                    missing: ph.short().to_string(),
                })?,
        )
    } else {
        None
    };

    // Get manifests
    let branch_manifest = manifest_for_commit_or_unavailable(
        repo.as_ref(),
        &branch_commit,
        &branch_name,
        &parent_name,
    )?;

    let parent_manifest = match &parent_commit {
        Some(c) => {
            manifest_for_commit_or_unavailable(repo.as_ref(), c, &branch_name, &parent_name)?
        }
        None => Manifest::empty(),
    };

    // Find the LCA (Lowest Common Ancestor) manifest
    let base_manifest = find_lca_manifest_with_parent(
        repo.as_ref(),
        &branch_name,
        &parent_name,
        parent_head.as_ref(),
    )?;

    // Pure path-by-path merge — emits clean entries plus a list of conflicts
    // resolved below (text → marker, binary → keep branch, etc.).
    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);

    let mut merged_entries = outcome.clean_entries;
    let resolution = resolve_conflicts_with_markers(
        repo.as_ref(),
        &base_manifest,
        outcome.conflicts,
        &branch_name,
        &parent_name,
    )?;
    merged_entries.extend(resolution.entries);
    let conflict_paths = resolution.conflict_paths;
    let binary_conflict_paths = resolution.binary_conflict_paths;

    let total_conflicts = conflict_paths.len() + binary_conflict_paths.len();

    if total_conflicts > 0 {
        // Write conflicted files to working directory
        let merged_manifest = Manifest::new(merged_entries);
        write_manifest_to_workdir(&lock, root, repo.as_ref(), &merged_manifest)?;

        // Save merge state in the backend's sidecar dir.
        let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
        let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");
        let merge_state_path = ctx.oak_dir.join("MERGE_STATE");
        repo.store_manifest(&merged_manifest)?;
        let state = MergeState {
            merged_manifest_hash: merged_manifest.hash.0.clone(),
            conflict_paths: conflict_paths
                .iter()
                .chain(binary_conflict_paths.iter())
                .cloned()
                .collect(),
        };
        let state_json = serde_json::to_string_pretty(&state)
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
        crate::atomic_file::write_atomic(&merge_state_path, state_json)?;
        fs::write(&merge_head_path, format!("{parent_name}\n{branch_name}"))?;
        fs::write(
            &merge_msg_path,
            format!("Merge branch '{branch_name}' into '{parent_name}'"),
        )?;

        output::warning(&format!(
            "Merge conflict: {total_conflicts} file(s) need manual resolution"
        ));
        for p in &conflict_paths {
            output::info(&format!("  CONFLICT (content): {p}"));
        }
        for p in &binary_conflict_paths {
            output::info(&format!("  CONFLICT (binary): {p} - kept branch version"));
        }
        output::info("");
        output::info("Fix the conflicts and then run 'oak merge --continue'");
        output::info("To abort the merge, run 'oak merge --abort'");

        return Err(OakError::MergeConflict(total_conflicts));
    }

    // No conflicts - complete the merge
    let merged_manifest = Manifest::new(merged_entries);
    complete_merge(
        &lock,
        repo.as_ref(),
        root,
        &branch_name,
        &parent_name,
        parent_head,
        Some(branch_head.clone()),
        &merged_manifest,
        true,
    )?;

    Ok(())
}

/// Resolution of the per-path conflicts a manifest-level 3-way merge emitted,
/// using the CLI's policy: text conflicts get diffy conflict markers (and only
/// count as conflicts when markers actually survive — non-overlapping edits
/// merge clean), binary conflicts keep the branch version, modify/delete keeps
/// the surviving side. Shared by `oak merge` and `oak pull`'s sync phase so
/// the two can't drift.
pub(crate) struct ConflictResolution {
    pub entries: Vec<ManifestEntry>,
    pub conflict_paths: Vec<String>,
    pub binary_conflict_paths: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MergeState {
    merged_manifest_hash: String,
    conflict_paths: Vec<String>,
}

pub(crate) fn resolve_conflicts_with_markers(
    repo: &dyn Repository,
    base_manifest: &Manifest,
    conflicts: Vec<MergeConflict>,
    branch_name: &str,
    parent_name: &str,
) -> Result<ConflictResolution> {
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut conflict_paths: Vec<String> = Vec::new();
    let mut binary_conflict_paths: Vec<String> = Vec::new();

    for MergeConflict {
        path,
        branch_entry,
        parent_entry,
    } in conflicts
    {
        match (branch_entry, parent_entry) {
            (Some(be), Some(pe)) => {
                // Both modified differently
                let bb = repo.get_blob(&be.blob_hash)?.ok_or_else(|| {
                    incomplete_blob_error(
                        &format!("conflict resolution for '{path}'"),
                        std::slice::from_ref(&be.blob_hash),
                    )
                })?;
                let pb = repo.get_blob(&pe.blob_hash)?.ok_or_else(|| {
                    incomplete_blob_error(
                        &format!("conflict resolution for '{path}'"),
                        std::slice::from_ref(&pe.blob_hash),
                    )
                })?;
                // The base (LCA) version is what makes diffy's 3-way merge
                // pinpoint just the diverging hunks. If the path didn't
                // exist in the LCA — added on both sides with different
                // content — there's no base to diff against, so feed an
                // empty string and let diffy treat the whole file as the
                // conflict.
                let base_text = base_manifest
                    .entries
                    .iter()
                    .find(|e| e.path == path)
                    .and_then(|e| repo.get_blob(&e.blob_hash).ok().flatten())
                    .and_then(|b| String::from_utf8(b.content).ok())
                    .unwrap_or_default();

                let branch_text = String::from_utf8(bb.content.clone());
                let parent_text = String::from_utf8(pb.content.clone());

                match (branch_text, parent_text) {
                    (Ok(bt), Ok(pt)) => {
                        let merged =
                            create_conflict_content(&base_text, &bt, &pt, branch_name, parent_name);
                        // Both sides editing the same file only means the
                        // manifest hashes diverge — NOT that the edits collide.
                        // diffy reconciles non-overlapping hunks cleanly (no
                        // markers); only flag a conflict when markers survive.
                        let had_conflict = content_has_conflict_markers(&merged);
                        let blob_hash = repo.put_blob(merged.into_bytes())?;
                        entries.push(ManifestEntry {
                            path: path.clone(),
                            blob_hash,
                            mode: be.mode,
                        });
                        if had_conflict {
                            conflict_paths.push(path);
                        }
                    }
                    _ => {
                        // Binary: keep branch version
                        entries.push(be);
                        binary_conflict_paths.push(path);
                    }
                }
            }
            (Some(be), None) => {
                // Parent deleted, branch modified
                entries.push(be);
                conflict_paths.push(path);
            }
            (None, Some(pe)) => {
                // Branch deleted, parent modified
                entries.push(pe);
                conflict_paths.push(path);
            }
            (None, None) => {
                // Both deleted — nothing to add (shouldn't appear in conflicts)
            }
        }
    }

    Ok(ConflictResolution {
        entries,
        conflict_paths,
        binary_conflict_paths,
    })
}

/// Push the current branch before a server-side squash-merge, so the server
/// merges the tip we actually have. Pushing also guarantees the server's
/// branch head equals the local one on return: the push protocol CAS-checks
/// the expected branch head and errors on divergence, and that error aborts
/// the merge.
async fn push_branch_for_merge(ctx: &crate::resolve::RepoContext, branch_name: &str) -> Result<()> {
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server(
            "Repository has no remote configured. Run `oak push` to link it to a server."
                .to_string(),
        )
    })?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        OakError::Server(
            "Repository isn't linked to an organization yet. Run `oak push` first.".to_string(),
        )
    })?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing repo name; re-clone with `oak clone <org>/<repo>`."
                .to_string(),
        )
    })?;
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    super::push::push_async(
        &repo,
        &ctx.work_tree,
        &remote,
        &owner,
        &repo_name,
        Some(branch_name),
        false,
        api_key.as_deref(),
    )
    .await
}

/// 3-way line merge of `branch_text` and `parent_text` against the common
/// ancestor `base_text`. Non-conflicting hunks come through clean; only the
/// diverging regions get wrapped in standard conflict markers labelled with
/// the branch names.
///
/// Returns the merged content whether or not it conflicted. Callers must NOT
/// assume the result is conflicted just because both sides touched the file:
/// diverging manifest hashes only mean both edited it, and diffy reconciles
/// non-overlapping edits into clean, marker-free output. Use
/// [`content_has_conflict_markers`] on the result to decide whether the path
/// is actually a conflict.
pub(crate) fn create_conflict_content(
    base_text: &str,
    branch_text: &str,
    parent_text: &str,
    branch_name: &str,
    parent_name: &str,
) -> String {
    let merged = match diffy::merge(base_text, branch_text, parent_text) {
        Ok(clean) => clean,
        Err(conflicted) => conflicted,
    };
    // diffy hardcodes the marker labels "ours" / "theirs" (and "original"
    // for the diff3 conflict style; we use the default Merge style which
    // omits it). Rewrite to the actual branch names so the user can tell
    // which side is which.
    merged
        .replace(
            &format!("{OURS_MARKER} ours"),
            &format!("{OURS_MARKER} {branch_name}"),
        )
        .replace(
            &format!("{THEIRS_MARKER} theirs"),
            &format!("{THEIRS_MARKER} {parent_name}"),
        )
}

// Conflict markers — kept as concat!() of two halves so that this very file
// (which contains the find_conflict_markers scanner below) doesn't trip its
// own naive substring check. The compiler still folds these to constants.
pub(crate) const OURS_MARKER: &str = concat!("<<<<", "<<<");
pub(crate) const THEIRS_MARKER: &str = concat!(">>>>", ">>>");

/// Find the LCA manifest between two branches by walking commit parent chains.
/// When the parent branch is `main` (which has no local branch row), pass its
/// head hash explicitly to ensure proper ancestor collection.
pub(crate) fn find_lca_manifest_with_parent(
    repo: &dyn Repository,
    branch_a: &str,
    branch_b: &str,
    branch_b_head: Option<&Hash>,
) -> Result<Manifest> {
    match resolve_merge_base(repo, branch_a, branch_b, branch_b_head)? {
        MergeBaseResolution::CommonAncestor(manifest)
        | MergeBaseResolution::DeclaredRoot(manifest) => Ok(manifest),
        MergeBaseResolution::Unavailable(reason) => Err(reason.to_error(branch_a, branch_b)),
    }
}

pub(crate) enum MergeBaseResolution {
    CommonAncestor(Manifest),
    DeclaredRoot(Manifest),
    Unavailable(MergeBaseUnavailable),
}

#[derive(Debug, Clone)]
pub(crate) enum MergeBaseUnavailable {
    IncompleteAncestry { missing: Vec<Hash> },
    IncompleteManifestData { missing: Vec<Hash> },
    IncompleteBlobData { missing: Vec<Hash> },
    NoVerifiedCommonAncestor,
}

impl MergeBaseUnavailable {
    pub(crate) fn to_error(&self, left: &str, right: &str) -> OakError {
        match self {
            MergeBaseUnavailable::IncompleteAncestry { missing } => {
                incomplete_ancestry_error(left, right, missing)
            }
            MergeBaseUnavailable::IncompleteManifestData { missing } => {
                incomplete_manifest_error(left, right, missing)
            }
            MergeBaseUnavailable::IncompleteBlobData { missing } => incomplete_blob_error(
                &format!("merge base between '{left}' and '{right}'"),
                missing,
            ),
            MergeBaseUnavailable::NoVerifiedCommonAncestor => OakError::NoVerifiedCommonAncestor {
                left: left.to_string(),
                right: right.to_string(),
            },
        }
    }
}

/// Resolve the merge base between two branches.
///
/// This is the central contract for merge-base availability:
/// - mutating callers convert [`MergeBaseUnavailable`] into a hard error;
/// - read-only prediction callers can degrade to "prediction unavailable";
/// - the only virtual base is the narrow, metadata-declared root-seeded branch
///   shape where Oak historically created a branch before its parent was
///   materialized locally.
pub(crate) fn resolve_merge_base(
    repo: &dyn Repository,
    branch_a: &str,
    branch_b: &str,
    branch_b_head: Option<&Hash>,
) -> Result<MergeBaseResolution> {
    // Collect ancestor hashes for both branches
    let a_walk = collect_ancestors(repo, branch_a)?;
    let b_walk = if let Some(head) = branch_b_head {
        collect_ancestors_from_head(repo, head)?
    } else {
        collect_ancestors(repo, branch_b)?
    };
    let b_ancestor_set: HashSet<String> = b_walk.ancestors.iter().map(|h| h.to_string()).collect();

    // The first ancestor of branch_a that also appears in branch_b's history is the LCA.
    for hash in &a_walk.ancestors {
        if b_ancestor_set.contains(&hash.to_string()) {
            let a_depth = a_walk.depth(hash).unwrap_or(usize::MAX);
            let b_depth = b_walk.depth(hash).unwrap_or(usize::MAX);
            let missing_edges = missing_edges_that_can_hide_lca(&a_walk, a_depth, &b_walk, b_depth);
            if !missing_edges.is_empty() {
                return Ok(MergeBaseResolution::Unavailable(
                    MergeBaseUnavailable::IncompleteAncestry {
                        missing: missing_edges,
                    },
                ));
            }
            let Some(commit) = repo.get_commit(hash)? else {
                return Ok(MergeBaseResolution::Unavailable(
                    MergeBaseUnavailable::IncompleteAncestry {
                        missing: vec![hash.clone()],
                    },
                ));
            };
            let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? else {
                return Ok(MergeBaseResolution::Unavailable(
                    MergeBaseUnavailable::IncompleteManifestData {
                        missing: vec![commit.manifest_hash.clone()],
                    },
                ));
            };
            let missing_blobs = missing_blobs_in_manifest(repo, &manifest)?;
            if !missing_blobs.is_empty() {
                return Ok(MergeBaseResolution::Unavailable(
                    MergeBaseUnavailable::IncompleteBlobData {
                        missing: missing_blobs,
                    },
                ));
            }
            return Ok(MergeBaseResolution::CommonAncestor(manifest));
        }
    }

    // Oak can have a branch whose metadata says it is parented to `branch_b`,
    // while its first local commit has no parent because the parent branch
    // (usually remote-only `main`) was not materialized locally when the
    // branch was created. In that narrow root-seeded shape, the empty
    // repository tree is the declared fork point. Keep this explicit and
    // metadata-gated so unrelated disjoint histories still fail clearly.
    if declared_root_base(repo, branch_a, branch_b, &a_walk.ancestors)? {
        return Ok(MergeBaseResolution::DeclaredRoot(Manifest::empty()));
    }

    let missing_edges = all_missing_edges(&a_walk, &b_walk);
    if !missing_edges.is_empty() {
        return Ok(MergeBaseResolution::Unavailable(
            MergeBaseUnavailable::IncompleteAncestry {
                missing: missing_edges,
            },
        ));
    }

    Ok(MergeBaseResolution::Unavailable(
        MergeBaseUnavailable::NoVerifiedCommonAncestor,
    ))
}

fn incomplete_ancestry_error(left: &str, right: &str, missing_edges: &[Hash]) -> OakError {
    let missing = format_missing_hashes(missing_edges);
    OakError::IncompleteAncestry {
        left: left.to_string(),
        right: right.to_string(),
        missing,
    }
}

pub(crate) fn incomplete_manifest_error(
    left: &str,
    right: &str,
    missing_manifests: &[Hash],
) -> OakError {
    let missing = format_missing_hashes(missing_manifests);
    OakError::IncompleteManifestData {
        left: left.to_string(),
        right: right.to_string(),
        missing,
    }
}

pub(crate) fn incomplete_blob_error(context: &str, missing_blobs: &[Hash]) -> OakError {
    let missing = format_missing_hashes(missing_blobs);
    OakError::IncompleteBlobData {
        context: context.to_string(),
        missing,
    }
}

fn format_missing_hashes(hashes: &[Hash]) -> String {
    let mut missing: Vec<String> = hashes.iter().map(|h| h.short().to_string()).collect();
    missing.sort();
    missing.dedup();
    missing.join(", ")
}

pub(crate) fn manifest_for_commit_or_unavailable(
    repo: &dyn Repository,
    commit: &Commit,
    left: &str,
    right: &str,
) -> Result<Manifest> {
    let manifest = repo.get_manifest(&commit.manifest_hash)?.ok_or_else(|| {
        incomplete_manifest_error(left, right, std::slice::from_ref(&commit.manifest_hash))
    })?;
    let missing_blobs = missing_blobs_in_manifest(repo, &manifest)?;
    if !missing_blobs.is_empty() {
        return Err(incomplete_blob_error(
            &format!("snapshot for '{left}' while merging with '{right}'"),
            &missing_blobs,
        ));
    }
    Ok(manifest)
}

fn missing_blobs_in_manifest(repo: &dyn Repository, manifest: &Manifest) -> Result<Vec<Hash>> {
    let mut missing_blobs = Vec::new();
    for entry in &manifest.entries {
        if repo.get_blob(&entry.blob_hash)?.is_none() {
            missing_blobs.push(entry.blob_hash.clone());
        }
    }
    Ok(missing_blobs)
}

fn declared_root_base(
    repo: &dyn Repository,
    branch_a: &str,
    branch_b: &str,
    a_ancestors: &[Hash],
) -> Result<bool> {
    let Some(branch) = repo.get_branch(branch_a)? else {
        return Ok(false);
    };
    if branch.parent_branch.as_deref() != Some(branch_b) {
        return Ok(false);
    }
    let mut root_count = 0usize;
    let mut branch_owned_root_count = 0usize;
    for hash in a_ancestors {
        let Some(commit) = repo.get_commit(hash)? else {
            continue;
        };
        if commit.parent_hash.is_none() && commit.merge_parent_hash.is_none() {
            root_count += 1;
            if commit.branch_name == branch_a {
                branch_owned_root_count += 1;
            }
            if root_count > 1 {
                return Ok(false);
            }
        }
    }
    Ok(root_count == 1 && branch_owned_root_count == 1)
}

#[derive(Debug, Default)]
struct AncestorWalk {
    ancestors: Vec<Hash>,
    depths: HashMap<String, usize>,
    missing_edges: Vec<MissingEdge>,
}

impl AncestorWalk {
    fn depth(&self, hash: &Hash) -> Option<usize> {
        self.depths.get(hash.as_str()).copied()
    }
}

#[derive(Debug)]
struct MissingEdge {
    hash: Hash,
    depth: usize,
}

fn missing_edges_that_can_hide_lca(
    a_walk: &AncestorWalk,
    a_candidate_depth: usize,
    b_walk: &AncestorWalk,
    b_candidate_depth: usize,
) -> Vec<Hash> {
    a_walk
        .missing_edges
        .iter()
        .filter(|edge| edge.depth <= a_candidate_depth)
        .chain(
            b_walk
                .missing_edges
                .iter()
                .filter(|edge| edge.depth <= b_candidate_depth),
        )
        .map(|edge| edge.hash.clone())
        .collect()
}

fn all_missing_edges(a_walk: &AncestorWalk, b_walk: &AncestorWalk) -> Vec<Hash> {
    a_walk
        .missing_edges
        .iter()
        .chain(b_walk.missing_edges.iter())
        .map(|edge| edge.hash.clone())
        .collect()
}

/// Collect ancestor commit hashes starting from a specific head hash using BFS.
/// Follows both parent_hash and merge_parent_hash to correctly traverse the DAG.
fn collect_ancestors_from_head(repo: &dyn Repository, head: &Hash) -> Result<AncestorWalk> {
    let mut ancestors = Vec::new();
    let mut depths = HashMap::new();
    let mut missing_edges = Vec::new();
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    queue.push_back((head.clone(), 0usize));

    while let Some((hash, depth)) = queue.pop_front() {
        if !visited.insert(hash.to_string()) {
            continue;
        }
        let Some(commit) = repo.get_commit(&hash)? else {
            missing_edges.push(MissingEdge { hash, depth });
            continue;
        };
        depths.insert(hash.to_string(), depth);
        ancestors.push(hash);
        if let Some(parent) = commit.parent_hash {
            queue.push_back((parent, depth + 1));
        }
        if let Some(merge_parent) = commit.merge_parent_hash {
            queue.push_back((merge_parent, depth + 1));
        }
    }

    Ok(AncestorWalk {
        ancestors,
        depths,
        missing_edges,
    })
}

/// Collect ancestor commit hashes for a branch using BFS.
/// Follows both parent_hash and merge_parent_hash to correctly
/// traverse the full DAG including merge commits.
fn collect_ancestors(repo: &dyn Repository, branch_name: &str) -> Result<AncestorWalk> {
    let head = repo.get_branch_head(branch_name)?;
    if let Some(h) = head {
        collect_ancestors_from_head(repo, &h)
    } else {
        Ok(AncestorWalk::default())
    }
}

/// Write manifest files to the working directory without deleting anything —
/// the conflict write-out. Untracked files and paths outside the manifest are
/// left alone (the user resolves conflicts in place and then `--continue`s).
/// Routed through the shared materializer, which also refreshes the stat
/// cache for every file written so a `oak status` run between writing the
/// conflicted tree and `--continue` doesn't trust a stale row.
pub(crate) fn write_manifest_to_workdir(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    manifest: &Manifest,
) -> Result<()> {
    apply_manifest(
        lock,
        root,
        repo,
        manifest,
        ApplyOpts {
            delete: DeleteScope::Nothing,
            clean_empty_dirs: false,
            ..ApplyOpts::default()
        },
    )
    .map(|_| ())
}

/// Complete the merge: create commit, update heads, close branch, switch
#[allow(clippy::too_many_arguments)]
fn complete_merge(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
    parent_name: &str,
    parent_head: Option<Hash>,
    branch_head: Option<Hash>,
    merged_manifest: &Manifest,
    update_workdir: bool,
) -> Result<()> {
    let parent_manifest = if let Some(ref ph) = parent_head {
        let pc = repo
            .get_commit(ph)?
            .ok_or_else(|| OakError::IncompleteCommitData {
                context: format!("parent head for '{parent_name}'"),
                missing: ph.short().to_string(),
            })?;
        manifest_for_commit_or_unavailable(repo, &pc, branch_name, parent_name)?
    } else {
        Manifest::empty()
    };

    let changes = parent_manifest.diff(merged_manifest);
    let file_changes: Vec<FileChange> = changes
        .iter()
        .map(|c| FileChange {
            path: c.path.clone(),
            change_type: c.change_type,
            old_blob_hash: c.old_blob_hash.clone(),
            new_blob_hash: c.new_blob_hash.clone(),
            old_path: c.old_path.clone(),
            old_mode: None,
            new_mode: None,
        })
        .collect();

    let author = super::commit::get_author();

    // Local merge commits don't carry messages: a real message only gets
    // attached when the server squash-merges a feature branch onto main,
    // and that path uses the source branch's description.
    let commit_hash = repo.put_commit_and_advance_refs(
        parent_name.to_string(),
        parent_head,
        branch_head,
        merged_manifest.entries.clone(),
        author,
        None,
        chrono::Utc::now(),
        file_changes,
    )?;

    if update_workdir {
        // Reset before removing or closing the source branch. If the worktree
        // update fails, the checkout still points at the branch being resolved.
        crate::commands::reset::reset_to_manifest(lock, root, repo, merged_manifest)?;
    }

    repo.set_current_branch(parent_name)?;
    // The merged-from branch's work now lives on the parent — drop its row
    // so it stops showing up in `oak switch` / `oak branch list`. Backends
    // without delete support (git sidecar) fall back to closing it; both
    // calls are metadata-only and tolerated as best-effort.
    let _ = repo.update_branch_status(branch_name, BranchStatus::Closed);
    let removed = repo.delete_branch(branch_name).is_ok();

    output::success(&format!(
        "Merged '{}{}{}' into '{}{}{}'",
        output::colors::BOLD,
        branch_name,
        output::colors::RESET,
        output::colors::BOLD,
        parent_name,
        output::colors::RESET,
    ));
    output::item(&format!(
        "commit {}{}{}",
        output::colors::CYAN,
        commit_hash.short(),
        output::colors::RESET,
    ));
    output::item(&format!(
        "branch '{}' {}, switched to '{}{}{}'",
        branch_name,
        if removed { "removed" } else { "closed" },
        output::colors::BOLD,
        parent_name,
        output::colors::RESET,
    ));

    Ok(())
}

fn load_merge_state(repo: &dyn Repository, path: &Path) -> Result<Option<MergeState>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    let state: MergeState = serde_json::from_str(&raw).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "invalid merge state '{}': {e}",
            path.display()
        )))
    })?;
    if repo
        .get_manifest(&Hash(state.merged_manifest_hash.clone()))?
        .is_none()
    {
        return Err(incomplete_manifest_error(
            "merge state",
            "recorded merge",
            std::slice::from_ref(&Hash(state.merged_manifest_hash)),
        ));
    }
    Ok(Some(state))
}

fn recompute_merge_state(
    repo: &dyn Repository,
    branch_name: &str,
    parent_name: &str,
) -> Result<MergeState> {
    let branch_head = repo
        .get_branch_head(branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.to_string()))?;
    let parent_head = repo
        .get_branch_head(parent_name)?
        .ok_or_else(|| OakError::BranchNotFound(parent_name.to_string()))?;
    let branch_commit =
        repo.get_commit(&branch_head)?
            .ok_or_else(|| OakError::IncompleteCommitData {
                context: format!("branch head for '{branch_name}'"),
                missing: branch_head.short().to_string(),
            })?;
    let parent_commit =
        repo.get_commit(&parent_head)?
            .ok_or_else(|| OakError::IncompleteCommitData {
                context: format!("parent head for '{parent_name}'"),
                missing: parent_head.short().to_string(),
            })?;
    let branch_manifest =
        manifest_for_commit_or_unavailable(repo, &branch_commit, branch_name, parent_name)?;
    let parent_manifest =
        manifest_for_commit_or_unavailable(repo, &parent_commit, branch_name, parent_name)?;
    let base_manifest =
        find_lca_manifest_with_parent(repo, branch_name, parent_name, Some(&parent_head))?;

    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);
    let mut merged_entries = outcome.clean_entries;
    let resolution = resolve_conflicts_with_markers(
        repo,
        &base_manifest,
        outcome.conflicts,
        branch_name,
        parent_name,
    )?;
    merged_entries.extend(resolution.entries);
    let merged_manifest = Manifest::new(merged_entries);
    repo.store_manifest(&merged_manifest)?;

    Ok(MergeState {
        merged_manifest_hash: merged_manifest.hash.0,
        conflict_paths: resolution
            .conflict_paths
            .into_iter()
            .chain(resolution.binary_conflict_paths)
            .collect(),
    })
}

#[allow(clippy::too_many_arguments)]
fn scope_merge_commit_to_recorded_merge(
    _lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    ignore: &IgnorePatterns,
    merged: &Manifest,
    scanned: &Manifest,
    conflict_paths: &[String],
    branch_name: &str,
    parent_name: &str,
    branch_head: Option<&Hash>,
) -> Result<Manifest> {
    use std::collections::HashMap;

    let conflicts: HashSet<&str> = conflict_paths.iter().map(|s| s.as_str()).collect();
    let disk: HashMap<&str, &ManifestEntry> = scanned
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let merged_by_path: HashMap<&str, &ManifestEntry> = merged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut entries = Vec::with_capacity(merged.entries.len());
    for entry in &merged.entries {
        if conflicts.contains(entry.path.as_str()) {
            if let Some(resolved) = disk.get(entry.path.as_str()) {
                entries.push((*resolved).clone());
            }
        } else {
            entries.push(entry.clone());
        }
    }
    let final_manifest = Manifest::new(entries);
    let final_paths: HashSet<&str> = final_manifest
        .entries
        .iter()
        .map(|e| e.path.as_str())
        .collect();

    let mut deleted: HashSet<String> = HashSet::new();
    if let Some(bh) = branch_head {
        let branch_commit = repo
            .get_commit(bh)?
            .ok_or_else(|| OakError::IncompleteCommitData {
                context: format!("branch head for '{branch_name}'"),
                missing: bh.short().to_string(),
            })?;
        let branch_manifest =
            manifest_for_commit_or_unavailable(repo, &branch_commit, branch_name, parent_name)?;
        for entry in &branch_manifest.entries {
            if final_paths.contains(entry.path.as_str()) {
                continue;
            }
            let Some(on_disk) = disk.get(entry.path.as_str()) else {
                continue;
            };
            if on_disk.blob_hash != entry.blob_hash || on_disk.mode != entry.mode {
                continue;
            }
            let file_path = root.join(&entry.path);
            match fs::symlink_metadata(&file_path) {
                Ok(md) if !md.is_dir() => fs::remove_file(&file_path)?,
                _ => continue,
            }
            deleted.insert(entry.path.clone());
        }
    }
    if !deleted.is_empty() {
        let deleted_list: Vec<String> = deleted.iter().cloned().collect();
        repo.update_stat_cache(&[], &deleted_list)?;
        crate::materialize::prune_emptied_dirs(root, ignore, &deleted_list)?;
    }

    let mut dirty = Vec::new();
    for entry in &scanned.entries {
        if conflicts.contains(entry.path.as_str()) || deleted.contains(&entry.path) {
            continue;
        }
        match merged_by_path.get(entry.path.as_str()) {
            Some(m) if m.blob_hash == entry.blob_hash && m.mode == entry.mode => {}
            _ => dirty.push(entry.path.clone()),
        }
    }
    for entry in &merged.entries {
        if !conflicts.contains(entry.path.as_str()) && !disk.contains_key(entry.path.as_str()) {
            dirty.push(format!("{} (locally deleted)", entry.path));
        }
    }

    if !dirty.is_empty() {
        dirty.sort();
        output::warning(&format!(
            "{} file(s) with local changes unrelated to this merge were NOT included in the merge commit and remain uncommitted:",
            dirty.len()
        ));
        const LIST_LIMIT: usize = 10;
        for path in dirty.iter().take(LIST_LIMIT) {
            output::info(&format!("  {path}"));
        }
        if dirty.len() > LIST_LIMIT {
            output::info(&format!("  … and {} more", dirty.len() - LIST_LIMIT));
        }
        output::info("Review them with 'oak status'; a plain 'oak commit' will include them.");
    }

    Ok(final_manifest)
}

/// Continue a merge after conflicts have been resolved
fn merge_continue(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
    let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");
    let merge_state_path = ctx.oak_dir.join("MERGE_STATE");

    if !merge_head_path.exists() {
        return Err(OakError::NoMergeInProgress);
    }

    // Read merge state
    let merge_head_content = fs::read_to_string(&merge_head_path)?;
    let mut lines = merge_head_content.lines();
    let parent_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();
    let branch_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();

    let ignore = IgnorePatterns::new(root)?;
    let merge_state = match load_merge_state(repo.as_ref(), &merge_state_path)? {
        Some(state) => state,
        None => match recompute_merge_state(repo.as_ref(), &branch_name, &parent_name) {
            Ok(state) => state,
            Err(
                e @ (OakError::IncompleteAncestry { .. }
                | OakError::IncompleteManifestData { .. }
                | OakError::IncompleteCommitData { .. }
                | OakError::IncompleteBlobData { .. }
                | OakError::NoVerifiedCommonAncestor { .. }),
            ) => return Err(e),
            Err(OakError::MergeFailed(msg)) if msg.contains("no verified common ancestor") => {
                return Err(OakError::MergeFailed(msg));
            }
            Err(e) => {
                return Err(OakError::MergeFailed(format!(
                    "merge state is missing and the merge result could not be reconstructed: {e}"
                )));
            }
        },
    };

    let conflicted = find_conflict_marker_delimiters_in_paths(root, &merge_state.conflict_paths)?;

    if !conflicted.is_empty() {
        output::error(&format!(
            "{} file(s) still have conflict markers:",
            conflicted.len()
        ));
        for p in &conflicted {
            output::info(&format!("  {p}"));
        }
        output::info("Edit these files to resolve conflicts, then run 'oak merge --continue'");
        return Err(OakError::MergeConflict(conflicted.len()));
    }

    // Re-hash the working tree from disk, then scope the merge commit to the
    // recorded merge result plus the user's conflict resolutions.
    let entries =
        crate::commands::commit::scan_working_dir_no_cache(root, root, repo.as_ref(), &ignore)?;
    let scanned = Manifest::new(entries);

    let parent_head = repo.get_branch_head(&parent_name)?;
    let branch_head = repo.get_branch_head(&branch_name)?;
    let recorded = repo
        .get_manifest(&Hash(merge_state.merged_manifest_hash.clone()))?
        .ok_or_else(|| {
            OakError::MergeFailed(format!(
                "merge state references missing manifest {}",
                merge_state.merged_manifest_hash
            ))
        })?;
    let merged_manifest = scope_merge_commit_to_recorded_merge(
        &lock,
        repo.as_ref(),
        root,
        &ignore,
        &recorded,
        &scanned,
        &merge_state.conflict_paths,
        &branch_name,
        &parent_name,
        branch_head.as_ref(),
    )?;

    complete_merge(
        &lock,
        repo.as_ref(),
        root,
        &branch_name,
        &parent_name,
        parent_head,
        branch_head,
        &merged_manifest,
        false,
    )?;

    // Clean up merge state files
    fs::remove_file(&merge_head_path)?;
    fs::remove_file(&merge_msg_path)?;
    fs::remove_file(&merge_state_path).ok();

    Ok(())
}

/// Abort a merge in progress
fn merge_abort(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
    let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");
    let merge_state_path = ctx.oak_dir.join("MERGE_STATE");

    if !merge_head_path.exists() {
        return Err(OakError::NoMergeInProgress);
    }

    let restore_status = abort_recorded_merge_state(
        &lock,
        root,
        repo.as_ref(),
        &merge_head_path,
        &merge_msg_path,
        &merge_state_path,
    );

    if restore_status == AbortRestoreStatus::Restored {
        output::success("Merge aborted, working directory restored");
    } else if restore_status == AbortRestoreStatus::PartiallyRestored {
        output::success(
            "Merge aborted; working directory partially restored and untracked files preserved",
        );
    } else {
        output::success("Merge aborted; working directory left unchanged");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbortRestoreStatus {
    Restored,
    PartiallyRestored,
    LeftUnchanged,
}

pub(crate) fn abort_recorded_merge_state(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    head_path: &Path,
    msg_path: &Path,
    state_path: &Path,
) -> AbortRestoreStatus {
    let head_content = match fs::read_to_string(head_path) {
        Ok(content) => content,
        Err(err) => {
            output::warning(&format!(
                "Could not read in-progress state '{}': {err}; state files will be cleared and the working tree left unchanged.",
                head_path.display()
            ));
            clear_in_progress_state(head_path, msg_path, state_path);
            return AbortRestoreStatus::LeftUnchanged;
        }
    };
    let mut lines = head_content.lines();
    let parent_name = lines.next().unwrap_or("_abort").to_string();
    let Some(branch_name) = lines.next().map(str::to_string) else {
        output::warning(
            "In-progress state is corrupt; state files will be cleared and the working tree left unchanged.",
        );
        clear_in_progress_state(head_path, msg_path, state_path);
        return AbortRestoreStatus::LeftUnchanged;
    };

    let recorded_manifest = abort_recorded_manifest(repo, state_path);
    let restore_status = restore_abort_worktree(
        lock,
        root,
        repo,
        &branch_name,
        &parent_name,
        recorded_manifest,
    );
    clear_in_progress_state(head_path, msg_path, state_path);
    restore_status
}

fn restore_abort_worktree(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    branch_name: &str,
    parent_name: &str,
    recorded_manifest: Option<Manifest>,
) -> AbortRestoreStatus {
    let head_hash = match repo.get_branch_head(branch_name) {
        Ok(head) => head,
        Err(err) => {
            output::warning(&format!(
                "Could not read current branch HEAD; state was cleared, but the working tree was left unchanged: {err}"
            ));
            return AbortRestoreStatus::LeftUnchanged;
        }
    };
    let Some(head_hash) = head_hash else {
        output::warning(
            "Current branch has no HEAD; state was cleared, but the working tree was left unchanged.",
        );
        return AbortRestoreStatus::LeftUnchanged;
    };
    let commit = match repo.get_commit(&head_hash) {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            output::warning(&format!(
                "Current branch HEAD commit {} is missing locally; state was cleared, but the working tree was left unchanged.",
                head_hash.short()
            ));
            output::warning(
                "Run `oak fetch` or `oak pull --force` to repair local history before committing.",
            );
            return AbortRestoreStatus::LeftUnchanged;
        }
        Err(err) => {
            output::warning(&format!(
                "Could not load current branch HEAD commit; state was cleared, but the working tree was left unchanged: {err}"
            ));
            return AbortRestoreStatus::LeftUnchanged;
        }
    };
    let manifest = match manifest_for_commit_or_unavailable(repo, &commit, branch_name, parent_name)
    {
        Ok(manifest) => manifest,
        Err(
            err @ (OakError::IncompleteManifestData { .. }
            | OakError::IncompleteCommitData { .. }
            | OakError::IncompleteBlobData { .. }),
        ) => {
            output::warning(&format!(
                "Current branch snapshot is incomplete locally; state was cleared, but the working tree was left unchanged: {err}"
            ));
            output::warning(
                "Run `oak fetch` or `oak pull --force` to repair local history before committing.",
            );
            return AbortRestoreStatus::LeftUnchanged;
        }
        Err(err) => {
            output::warning(&format!(
                "Could not load current branch snapshot; state was cleared, but the working tree was left unchanged: {err}"
            ));
            return AbortRestoreStatus::LeftUnchanged;
        }
    };
    let delete = recorded_manifest
        .as_ref()
        .map(|old| DeleteScope::TrackedRemoved { old })
        .unwrap_or(DeleteScope::Nothing);
    match apply_manifest(
        lock,
        root,
        repo,
        &manifest,
        ApplyOpts {
            delete,
            ..ApplyOpts::default()
        },
    ) {
        Ok(_) if recorded_manifest.is_some() => AbortRestoreStatus::Restored,
        Ok(_) => {
            output::warning(
                "Recorded merge result was unavailable; merge-added paths may remain in the working tree.",
            );
            AbortRestoreStatus::PartiallyRestored
        }
        Err(err) => {
            output::warning(&format!(
                "Could not restore the working tree while aborting; state was cleared, but files may still contain conflict content: {err}"
            ));
            AbortRestoreStatus::PartiallyRestored
        }
    }
}

fn clear_in_progress_state(head_path: &Path, msg_path: &Path, state_path: &Path) {
    fs::remove_file(head_path).ok();
    fs::remove_file(msg_path).ok();
    fs::remove_file(state_path).ok();
}

fn abort_recorded_manifest(repo: &dyn Repository, state_path: &Path) -> Option<Manifest> {
    if !state_path.exists() {
        return None;
    }
    let raw = match fs::read_to_string(state_path) {
        Ok(raw) => raw,
        Err(err) => {
            output::warning(&format!(
                "Could not read merge state for tracked cleanup: {err}"
            ));
            return None;
        }
    };
    #[derive(Deserialize)]
    struct RecordedState {
        merged_manifest_hash: String,
    }
    let state: RecordedState = match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(err) => {
            output::warning(&format!(
                "Could not parse merge state for tracked cleanup: {err}"
            ));
            return None;
        }
    };
    let manifest_hash = Hash(state.merged_manifest_hash);
    match repo.get_manifest(&manifest_hash) {
        Ok(Some(manifest)) => Some(manifest),
        Ok(None) => {
            output::warning(&format!(
                "Recorded merge manifest {} is missing; preserving untracked and merge-added paths.",
                manifest_hash.short()
            ));
            None
        }
        Err(err) => {
            output::warning(&format!(
                "Could not load merge manifest for tracked cleanup: {err}"
            ));
            None
        }
    }
}

/// Recursively scan for files containing conflict markers
pub(crate) fn find_conflict_markers(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
) -> Result<Vec<String>> {
    let mut conflicted = Vec::new();

    if !dir.exists() {
        return Ok(conflicted);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();

        if ignore.is_ignored(relative, file_type.is_dir()) {
            continue;
        }

        if file_type.is_dir() {
            conflicted.extend(find_conflict_markers(&path, root, ignore)?);
        } else if file_type.is_file() {
            if let Ok(content) = fs::read_to_string(&path) {
                if content_has_conflict_markers(&content) {
                    conflicted.push(relative.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(conflicted)
}

pub(crate) fn find_conflict_marker_delimiters(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
) -> Result<Vec<String>> {
    let mut conflicted = Vec::new();

    if !dir.exists() {
        return Ok(conflicted);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();

        if ignore.is_ignored(relative, file_type.is_dir()) {
            continue;
        }

        if file_type.is_dir() {
            conflicted.extend(find_conflict_marker_delimiters(&path, root, ignore)?);
        } else if file_type.is_file() {
            if let Ok(content) = fs::read_to_string(&path) {
                if content_has_conflict_marker_delimiter(&content) {
                    conflicted.push(relative.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(conflicted)
}

pub(crate) fn find_conflict_marker_delimiters_in_paths(
    root: &Path,
    paths: &[String],
) -> Result<Vec<String>> {
    let mut conflicted = Vec::new();
    for rel in paths {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(OakError::InvalidPath(format!(
                "merge state path '{rel}' is not inside this checkout"
            )));
        }
        let path = root.join(rel_path);
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.file_type().is_file() {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
            if content_has_conflict_marker_delimiter(&content) {
                conflicted.push(rel.clone());
            }
        }
    }
    Ok(conflicted)
}

/// True if `content` still contains unresolved 3-way conflict markers.
///
/// Real markers are written at column 0 (see `create_conflict_content`), so we
/// anchor the check to line starts. A plain `contains` would false-positive on
/// source that merely mentions the marker strings in doc comments, test
/// fixtures, or user-facing error messages — none of which are at column 0 —
/// and would refuse to complete an otherwise-resolved merge.
pub(crate) fn content_has_conflict_markers(content: &str) -> bool {
    let has_ours = content.lines().any(|l| l.starts_with(OURS_MARKER));
    let has_theirs = content.lines().any(|l| l.starts_with(THEIRS_MARKER));
    has_ours && has_theirs
}

/// True if `content` contains any conflict marker delimiter at column 0.
///
/// Continue paths use this stricter predicate so a hand-edited file with a
/// stray `<<<<<<<`, `=======`, or `>>>>>>>` line cannot be committed as a
/// supposedly resolved conflict.
pub(crate) fn content_has_conflict_marker_delimiter(content: &str) -> bool {
    content.lines().any(|line| {
        line.starts_with(OURS_MARKER)
            || line.starts_with("=======")
            || line.starts_with(THEIRS_MARKER)
    })
}

#[derive(Deserialize)]
struct MergeResponse {
    message: String,
    commit_hash: String,
    // Shape of the squash commit the server just landed on `main`. Present
    // on current servers; absent (defaulted to None) when talking to an
    // older server, in which case the caller falls back to fetching main's
    // tree. See `try_ingest_merged_main`.
    #[serde(default)]
    manifest_hash: Option<String>,
    #[serde(default)]
    parent_hash: Option<String>,
    #[serde(default)]
    merge_parent_hash: Option<String>,
}

/// Squash-merge a branch onto `main` by delegating to the server. The
/// server is the only place `main` exists and is the only writer that
/// uses the branch description as the merge commit's message.
///
/// After the server merge lands, this also auto-syncs the new personal
/// branch to main's fresh HEAD: it pulls the squash commit and manifest
/// into local storage, points the new branch at that head, and resets
/// the working tree to match. Without this, the new local branch would
/// sit on the old branch tip — out of sync with main, unreachable from
/// `oak pull`, and effectively guaranteed to "conflict on the entire
/// file" on the next `oak sync` because the LCA finder couldn't locate
/// a common ancestor.
pub(crate) async fn merge_to_main_via_server(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
) -> Result<()> {
    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server(
            "Repository has no remote configured. Run `oak push` to link it to a server."
                .to_string(),
        )
    })?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        OakError::Server(
            "Repository isn't linked to an organization yet. Run `oak push` first.".to_string(),
        )
    })?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing repo name; re-clone with `oak clone <org>/<repo>`."
                .to_string(),
        )
    })?;

    let token = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    // Server mounts repo routes directly under `/api` (no `repos/` prefix):
    // see `build_api_routes` in oak-server/src/lib.rs. The trailing-slash trim
    // keeps a `https://oak.space/` remote from producing a `//api/...` URL.
    let url = format!(
        "{remote}/api/{owner}/{repo_name}/branches/{}/merge",
        super::branch_api_segment(branch_name),
        remote = remote.trim_end_matches('/'),
    );

    let client = crate::http::api_client();
    let mut req = client.post(&url);
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_merge_conflict_error(branch_name, &body));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = crate::http::error_text(resp).await;
        // The server refuses to merge a branch it has already closed — merged
        // here earlier but the local row never caught up, merged in the web UI,
        // or landed by another agent — with `400 … is already closed`. The work
        // is already on main, so don't dead-end the user with an error: recover
        // the way `oak pull` does and switch them onto a fresh branch.
        if status == reqwest::StatusCode::BAD_REQUEST
            && body.to_lowercase().contains("already closed")
        {
            return recover_already_closed_branch(lock, repo, root, branch_name).await;
        }
        return Err(OakError::Server(format!("Merge failed: {body}")));
    }

    let merged: MergeResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    output::success(&format!("Merged '{branch_name}' onto main"));
    output::detail("commit", &merged.commit_hash);
    output::detail("message", &merged.message);

    // The server closed the branch as part of the merge — mirror that
    // locally so `oak switch` matches server state.
    if let Err(e) = repo.update_branch_status(branch_name, BranchStatus::Closed) {
        output::warning(&format!(
            "Local branch state not updated (merge still succeeded on server): {e}"
        ));
    }

    // Bring the local copy of `main` up to the squash commit the server
    // just landed. The working tree we're holding is stale the moment the
    // merge returns, and storing the squash commit locally is what lets
    // the LCA finder behave correctly the next time a descendant runs
    // `oak pull`.
    //
    // Fast path: a clean merge of a branch that was up to date with main
    // produces a merged tree that is byte-for-byte the manifest this
    // branch already holds (content-addressing). When that manifest is
    // already in local storage we reconstruct main's new HEAD from the
    // hashes in the merge response — no network round-trips, no re-walking
    // main's whole tree.
    //
    // Slow path: only when the merged manifest is new to us (main moved
    // under the branch, so the 3-way merge produced a tree we've never
    // stored) do we fetch it from the server. If that fetch fails (network
    // blip, server glitch), the merge still succeeded on the server, so we
    // warn and fall back to pinning the new branch at the old branch tip;
    // `oak pull` recovers once connectivity returns.
    let new_main_head = match try_ingest_merged_main(repo, &merged)? {
        Some(h) => Some(h),
        None => match super::sync::fetch_parent_from_server(repo, "main").await {
            Ok(Some(h)) => Some(h),
            Ok(None) => None,
            Err(e) => {
                output::warning(&format!(
                    "Couldn't refresh main locally (merge succeeded on server): {e}"
                ));
                None
            }
        },
    };

    // The branch's work is now on main. Retire it and switch the user onto a
    // fresh branch parented on main — seeded at main's new HEAD when auto-sync
    // produced one, otherwise at the just-merged branch's tip (worktree left
    // as-is). See `switch_to_fresh_main_branch`.
    switch_to_fresh_main_branch(lock, repo, root, branch_name, new_main_head)
}

/// Retire `branch_name` and move the user onto a fresh personal branch
/// parented on main.
///
/// With `new_main_head` known (auto-sync after a server merge, or a successful
/// main refresh during recovery) the new branch seeds there and the working
/// tree is reset to match — a clean start in perfect sync with main. Without
/// it, the new branch seeds at the retired branch's own tip and the working
/// tree is left untouched, so nothing is silently discarded on the fallback
/// path.
fn switch_to_fresh_main_branch(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
    new_main_head: Option<Hash>,
) -> Result<()> {
    let new_branch_name = next_open_personal_branch_name(repo)?;
    let new_br = oak_core::Branch::new(new_branch_name.clone(), None, Some("main".to_string()));
    repo.store_branch(&new_br)?;

    let seed_head = if let Some(ref h) = new_main_head {
        Some(h.clone())
    } else {
        repo.get_branch_head(branch_name)?
    };
    if let Some(ref head) = seed_head {
        repo.set_branch_head(&new_branch_name, head)?;
        // Move HEAD onto the new branch's tip too. `set_current_branch`
        // alone leaves HEAD pointing at the just-closed feature branch's
        // commit, so the next `oak commit` would parent off the wrong
        // commit (and `oak hash` would lie). Mirror `oak switch`, which
        // always pairs the current-branch update with a `set_head`.
        repo.set_head(head)?;
    }
    repo.set_current_branch(&new_branch_name)?;

    // Reset the working tree to main's new manifest, but only when
    // auto-sync gave us a head whose commit + manifest are now in
    // local storage. The reset writes every file in the manifest and
    // deletes anything outside it, so we must skip it on the fallback
    // path — otherwise we'd blow away the user's working tree based on
    // an unknown manifest.
    if let Some(ref head) = new_main_head {
        if let Some(commit) = repo.get_commit(head)? {
            if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
                crate::commands::reset::reset_to_manifest(lock, root, repo, &manifest)?;
            }
        }
    }

    // The branch's work landed on main — drop the local row entirely (the
    // server keeps the authoritative closed record; local commits stay,
    // branch_name is a soft label). Backends without delete support keep
    // the closed row from above.
    let removed = repo.delete_branch(branch_name).is_ok();

    output::item(&format!(
        "branch '{branch_name}' {}, switched to new branch '{}{}{}'  (parented onto main)",
        if removed { "removed" } else { "closed" },
        output::colors::BOLD,
        new_branch_name,
        output::colors::RESET,
    ));

    Ok(())
}

/// Recover from a server merge rejected because the branch is already closed.
///
/// A `400 … is already closed` means the branch's work is already on main: it
/// was merged here earlier (and the local row never caught up), merged in the
/// web UI, or landed by another agent. Rather than surface a hard error,
/// mirror `oak pull` — refresh main, retire the closed branch, and switch the
/// user onto a fresh branch parented on main.
///
/// Only the *current* branch triggers the switch; an explicit
/// `oak merge <other-branch>` for an already-closed branch just reports the
/// fact, since yanking the working tree out from under a branch the user isn't
/// standing on would be surprising.
async fn recover_already_closed_branch(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
) -> Result<()> {
    let current_branch = repo.get_current_branch_name()?;
    if current_branch.as_deref() != Some(branch_name) {
        // Best-effort: mirror the server's closed state and drop the stale row
        // so it stops cluttering `oak switch`. Don't move the user.
        let _ = repo.update_branch_status(branch_name, BranchStatus::Closed);
        let _ = repo.delete_branch(branch_name);
        output::success(&format!(
            "Branch '{branch_name}' is already merged onto main — nothing to do."
        ));
        return Ok(());
    }

    // Refresh main so its history carries the squash commit whose
    // `merge_parent_hash` points at this branch's tip — the ancestry the
    // reconcile uses to prove the branch landed and to reset the worktree. We
    // want the side effect (main's commits in local storage), not the returned
    // head. (Don't mark the branch closed yet: the reconcile plan treats a
    // closed current branch as a no-op.)
    if let Err(e) = super::sync::fetch_parent_from_server(repo, "main").await {
        output::warning(&format!("Couldn't refresh main locally: {e}"));
    }

    // Preferred path: reconcile exactly like `oak pull`. It only resets the
    // working tree to main once it can prove from main's history that this
    // branch landed there, so it's safe even if the branch was closed without
    // merging. The merge flow already rejected a dirty tree, so a reset here
    // can't clobber uncommitted work.
    if let Some(plan) = plan_remote_merge_reconcile(repo, RemoteMergeReconcileScope::CurrentBranch)?
    {
        if let Some(reconciled) = apply_remote_merge_reconcile(lock, repo, root, true, plan)? {
            print_remote_merge_reconcile(&reconciled);
            return Ok(());
        }
    }

    // Fallback: main's local history doesn't prove the squash landed (an older
    // server, a closed-but-unmerged branch, or the refresh above failed). Still
    // honor the intent — get the user off the dead branch onto a fresh one
    // parented on main — but seed at the branch's own tip and leave the working
    // tree untouched so nothing is discarded.
    let _ = repo.update_branch_status(branch_name, BranchStatus::Closed);
    output::info(&format!(
        "Branch '{branch_name}' is already closed on the server; switching you to a fresh branch."
    ));
    switch_to_fresh_main_branch(lock, repo, root, branch_name, None)
}

/// Render a server-side squash-merge conflict (HTTP 409) the way the local
/// merge path renders its conflicts: name each file, then say how to fix it.
/// The server lists the conflicting paths in its error message ("… on both
/// branches: a, b"); a structured `conflict_paths` field wins when present.
/// Anything that doesn't match the server's envelope (a proxy error page,
/// an older server) falls back to the generic rendering.
fn server_merge_conflict_error(branch_name: &str, body: &str) -> OakError {
    #[derive(serde::Deserialize)]
    struct ConflictBody {
        error: String,
        #[serde(default)]
        conflict_paths: Vec<String>,
    }

    let Ok(parsed) = serde_json::from_str::<ConflictBody>(body.trim()) else {
        let body = body.trim();
        return OakError::Server(if body.is_empty() {
            "Merge failed: HTTP 409 Conflict".to_string()
        } else {
            format!("Merge failed: HTTP 409 Conflict: {body}")
        });
    };

    let paths = if parsed.conflict_paths.is_empty() {
        parsed
            .error
            .rsplit_once("both branches: ")
            .map(|(_, list)| list.split(", ").map(str::to_string).collect())
            .unwrap_or_default()
    } else {
        parsed.conflict_paths
    };
    if paths.is_empty() {
        return OakError::Server(format!("Merge failed: {}", parsed.error));
    }

    output::warning(&format!(
        "Merge conflict: {} file(s) changed on both '{branch_name}' and main",
        paths.len()
    ));
    for p in &paths {
        output::info(&format!("  CONFLICT: {p}"));
    }
    output::info("");
    output::info("Run 'oak pull' to merge main into this branch and resolve the conflicts,");
    output::info("then run 'oak merge' again.");

    OakError::MergeConflict(paths.len())
}

pub(crate) struct RemoteMergeReconcile {
    pub closed_branch: String,
    pub new_branch: String,
    pub reset_worktree: bool,
}

pub(crate) fn print_remote_merge_reconcile(reconciled: &RemoteMergeReconcile) {
    output::item(&format!(
        "branch '{}' was already merged on main; switched to new branch '{}{}{}'",
        reconciled.closed_branch,
        output::colors::BOLD,
        reconciled.new_branch,
        output::colors::RESET,
    ));
    if reconciled.reset_worktree {
        output::info("Updated working tree to latest main.");
    } else {
        output::warning("Preserved uncommitted changes on the new branch.");
    }
}

pub(crate) struct RemoteMergeReconcilePlan {
    branch_name: String,
    branch_head: Hash,
    main_head: Hash,
}

pub(crate) enum RemoteMergeReconcileScope<'a> {
    CurrentBranch,
    Pull {
        branch_open_at_start: Option<&'a str>,
    },
}

/// Mirror the local post-merge state when another client already squash-merged
/// the current branch onto `main`.
///
/// `oak fetch` / `oak status` can learn that fact after local `main` history is
/// refreshed: the squash commit on `main` carries the merged branch tip in
/// `merge_parent_hash`. When we see that ancestry locally, perform the same
/// branch transition as `oak merge`: close the old branch, create a fresh
/// branch parented onto `main`, point it at main's current head, and switch to
/// it. Clean worktrees are reset to main; dirty worktrees are left untouched so
/// uncommitted edits move onto the fresh branch instead of being discarded.
pub(crate) fn apply_remote_merge_reconcile(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    working_tree_clean: bool,
    plan: RemoteMergeReconcilePlan,
) -> Result<Option<RemoteMergeReconcile>> {
    let Some(current_branch) = repo.get_current_branch_name()? else {
        return Ok(None);
    };
    if current_branch != plan.branch_name {
        return Ok(None);
    }
    if repo.get_branch_head(&plan.branch_name)? != Some(plan.branch_head.clone()) {
        return Ok(None);
    }
    if repo.get_branch_head("main")? != Some(plan.main_head.clone()) {
        return Ok(None);
    }

    let main_manifest = if working_tree_clean {
        let Some(main_commit) = repo.get_commit(&plan.main_head)? else {
            return Ok(None);
        };
        let Some(main_manifest) = repo.get_manifest(&main_commit.manifest_hash)? else {
            return Ok(None);
        };
        Some(main_manifest)
    } else {
        None
    };

    let branch_name = plan.branch_name;
    let main_head = plan.main_head;

    let new_branch_name = next_open_personal_branch_name(repo)?;
    let new_br = oak_core::Branch::new(new_branch_name.clone(), None, Some("main".to_string()));
    repo.reconcile_remote_merge_branch_state(&branch_name, &new_br, &main_head)?;
    // The merged branch's work is on main now — drop the closed tombstone
    // the reconcile transaction left behind (best-effort; backends without
    // delete support keep the closed row).
    let _ = repo.delete_branch(&branch_name);

    if let Some(main_manifest) = main_manifest {
        crate::commands::reset::reset_to_manifest(lock, root, repo, &main_manifest)?;
    }

    Ok(Some(RemoteMergeReconcile {
        closed_branch: branch_name,
        new_branch: new_branch_name,
        reset_worktree: working_tree_clean,
    }))
}

pub(crate) fn plan_remote_merge_reconcile(
    repo: &dyn Repository,
    scope: RemoteMergeReconcileScope<'_>,
) -> Result<Option<RemoteMergeReconcilePlan>> {
    let allow_closed_for_branch = match scope {
        RemoteMergeReconcileScope::CurrentBranch => None,
        RemoteMergeReconcileScope::Pull {
            branch_open_at_start,
        } => match branch_open_at_start {
            Some(branch_name) => Some(branch_name),
            None => return Ok(None),
        },
    };

    let Some(branch_name) = repo.get_current_branch_name()? else {
        return Ok(None);
    };
    if branch_name.is_empty() || branch_name == "main" {
        return Ok(None);
    }

    let Some(branch) = repo.get_branch(&branch_name)? else {
        return Ok(None);
    };
    if branch.parent_branch.as_deref() != Some("main") {
        return Ok(None);
    }
    if let Some(expected_branch) = allow_closed_for_branch {
        if branch_name != expected_branch {
            return Ok(None);
        }
    }
    // Normal reconciliation is an Open -> Closed transition. `oak pull` is the
    // exception: it may fetch server metadata that marks the current branch
    // closed before main is refreshed, so pull passes the exact branch that was
    // open when the command began. Closed branches from any other path remain a
    // no-op to avoid manual-close and switch-back rotations.
    if branch.status == BranchStatus::Closed && allow_closed_for_branch.is_none() {
        return Ok(None);
    }

    let Some(branch_head) = repo.get_branch_head(&branch_name)? else {
        return Ok(None);
    };
    if !repo.merge_child_exists(&branch_head)? {
        return Ok(None);
    }

    let Some(main_head) = repo.get_branch_head("main")? else {
        return Ok(None);
    };

    let merged_into_main = main_history_contains_merge_parent(repo, &main_head, &branch_head)?;
    if !merged_into_main {
        return Ok(None);
    }

    Ok(Some(RemoteMergeReconcilePlan {
        branch_name,
        branch_head,
        main_head,
    }))
}

fn main_history_contains_merge_parent(
    repo: &dyn Repository,
    main_head: &Hash,
    merged_branch_head: &Hash,
) -> Result<bool> {
    const MAX_MAIN_WALK: usize = 100_000;

    let mut seen = HashSet::new();
    let mut current = Some(main_head.clone());
    let mut walked = 0usize;
    while let Some(hash) = current {
        if walked >= MAX_MAIN_WALK || !seen.insert(hash.clone()) {
            break;
        }
        walked += 1;

        let Some(commit) = repo.get_commit(&hash)? else {
            break;
        };
        if commit.merge_parent_hash.as_ref() == Some(merged_branch_head) {
            return Ok(true);
        }
        // Walk the first-parent `main` chain only. `merge_parent_hash` points
        // at merged feature branches; following it would leave main history and
        // turn unrelated feature ancestry into merge proof.
        current = commit.parent_hash;
    }

    Ok(false)
}

/// Collect the tip of every branch that `main`'s first-parent history
/// records as squash-merged: each squash commit carries the merged branch's
/// tip in `merge_parent_hash`. Only main's chain is walked — sync commits on
/// feature branches also carry `merge_parent_hash` (pointing at main), and
/// trusting those would let a fresh branch seeded at main's head look
/// "merged".
fn collect_main_merge_tips(repo: &dyn Repository) -> Result<HashSet<Hash>> {
    const MAX_MAIN_WALK: usize = 100_000;

    let mut tips = HashSet::new();
    let Some(main_head) = repo.get_branch_head("main")? else {
        return Ok(tips);
    };

    let mut seen = HashSet::new();
    let mut current = Some(main_head);
    let mut walked = 0usize;
    while let Some(hash) = current {
        if walked >= MAX_MAIN_WALK || !seen.insert(hash.clone()) {
            break;
        }
        walked += 1;
        let Some(commit) = repo.get_commit(&hash)? else {
            break;
        };
        if let Some(tip) = commit.merge_parent_hash {
            tips.insert(tip);
        }
        current = commit.parent_hash;
    }
    Ok(tips)
}

/// Delete local rows for branches whose work already landed on `main` —
/// merged by this client in an earlier session, by another client, or in the
/// web UI. A branch is pruned when its head appears as a `merge_parent_hash`
/// in main's local history (the squash commit is the proof), or when it is
/// closed with no head at all. A branch with commits past its merged tip
/// keeps its row: the head no longer matches any squash commit, so the
/// unmerged work is never discarded. The current branch is always kept —
/// `apply_remote_merge_reconcile` owns that rotation.
///
/// Returns the pruned branch names. No-ops (returning what was pruned so
/// far) on backends without delete support.
pub(crate) fn prune_merged_branches(repo: &dyn Repository) -> Result<Vec<String>> {
    let merge_tips = collect_main_merge_tips(repo)?;
    let current = repo.get_current_branch_name()?;

    let mut pruned = Vec::new();
    for br in repo.list_branches()? {
        if br.name == oak_core::DEFAULT_BRANCH || current.as_deref() == Some(br.name.as_str()) {
            continue;
        }
        let merged = match repo.get_branch_head(&br.name)? {
            Some(head) => merge_tips.contains(&head),
            // A closed branch with no head has no commits to lose.
            None => br.status == BranchStatus::Closed,
        };
        if !merged {
            continue;
        }
        match repo.delete_branch(&br.name) {
            Ok(()) => pruned.push(br.name),
            Err(OakError::Unsupported { .. }) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(pruned)
}

pub(crate) fn print_pruned_branches(pruned: &[String]) {
    for name in pruned {
        output::item(&format!("removed merged branch '{name}'"));
    }
}

/// Reconstruct `main`'s new HEAD locally from a merge response, without
/// any network round-trips, when we already have everything we need.
///
/// The merge response carries the squash commit's hashes. When its
/// manifest is already in local storage — true whenever the merged tree
/// equals a manifest we've stored, i.e. the usual clean merge of an
/// up-to-date branch — we can synthesize the `main` commit row, ensure the
/// `main` branch row, and point it at the new head. By our storage
/// invariant a manifest is only ever stored after the blobs it references,
/// so holding the manifest means we hold the content too: no blob fetch,
/// no tree walk.
///
/// Returns `Some(head)` when the fast path applied, or `None` when the
/// caller should fall back to fetching main's tree from the server — the
/// merged manifest is new to us (main moved under the branch), or the
/// response came from an older server that didn't send the manifest hash.
///
/// Mirrors the commit-row synthesis in
/// [`super::sync::fetch_parent_from_server`]: the row's hash is the
/// server's authoritative commit hash, and `parent_hash` /
/// `merge_parent_hash` are carried through so the LCA finder can walk the
/// chain on later merges. Author / timestamp are placeholders, but message
/// and file rows are display metadata that keep `oak log` honest.
fn try_ingest_merged_main(repo: &dyn Repository, merged: &MergeResponse) -> Result<Option<Hash>> {
    let Some(manifest_hash) = merged.manifest_hash.as_ref() else {
        return Ok(None);
    };
    let manifest_hash = Hash(manifest_hash.clone());
    if repo.get_manifest(&manifest_hash)?.is_none() {
        return Ok(None);
    }

    let head_hash = Hash(merged.commit_hash.clone());
    // Idempotent: a retry that already stored this commit just re-points
    // the branch row below.
    if repo.get_commit(&head_hash)?.is_none() {
        let parent_hash = merged.parent_hash.clone().map(Hash);
        let files =
            super::sync::files_for_remote_commit(repo, parent_hash.as_ref(), &manifest_hash)?;
        let commit = Commit {
            hash: head_hash.clone(),
            branch_name: "main".to_string(),
            parent_hash,
            merge_parent_hash: merged.merge_parent_hash.clone().map(Hash),
            manifest_hash,
            author: "<remote>".to_string(),
            message: Some(merged.message.clone()),
            timestamp: chrono::Utc::now(),
            files,
        };
        repo.store_commit(&commit)?;
    }

    if repo.get_branch("main")?.is_none() {
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))?;
    }
    repo.set_branch_head("main", &head_hash)?;
    Ok(Some(head_hash))
}

/// Pick a name for a fresh personal branch that doesn't collide with any
/// existing branch row. Tries the author-derived default first, then
/// appends `-2`, `-3`, ... If everything up to `-99` is taken (vanishingly
/// unlikely), falls back to a timestamp suffix.
pub(crate) fn next_open_personal_branch_name(repo: &dyn Repository) -> Result<String> {
    let base = super::init::default_local_branch_name();
    if repo.get_branch(&base)?.is_none() {
        return Ok(base);
    }
    for n in 2..100 {
        let candidate = format!("{base}-{n}");
        if repo.get_branch(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", chrono::Utc::now().timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_conflict_paths_parsed_from_message() {
        let body = r#"{"error":"Merge conflict: 2 file(s) have overlapping edits on both branches: src/a.rs, src/b.rs"}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::MergeConflict(n) => assert_eq!(n, 2),
            other => panic!("expected MergeConflict, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_structured_paths_win_over_message() {
        let body =
            r#"{"error":"Merge conflict: stale message","conflict_paths":["x.rs","y.rs","z.rs"]}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::MergeConflict(n) => assert_eq!(n, 3),
            other => panic!("expected MergeConflict, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_without_paths_keeps_server_message() {
        let body = r#"{"error":"Merge conflict: branch is read-only"}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::Server(msg) => {
                assert_eq!(msg, "Merge failed: Merge conflict: branch is read-only")
            }
            other => panic!("expected Server, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_non_json_body_falls_back_to_generic() {
        match server_merge_conflict_error("feature", "<html>oops</html>") {
            OakError::Server(msg) => {
                assert_eq!(msg, "Merge failed: HTTP 409 Conflict: <html>oops</html>")
            }
            other => panic!("expected Server, got: {other}"),
        }
        match server_merge_conflict_error("feature", "  ") {
            OakError::Server(msg) => assert_eq!(msg, "Merge failed: HTTP 409 Conflict"),
            other => panic!("expected Server, got: {other}"),
        }
    }

    #[test]
    fn ingest_merged_main_preserves_message_and_file_rows() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
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
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let (feature_entry, _feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let merged_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let server_head = "ab".repeat(32);
        let merged = MergeResponse {
            message: "ship feature".to_string(),
            commit_hash: server_head.clone(),
            manifest_hash: Some(merged_manifest_hash.to_string()),
            parent_hash: Some(base_head.to_string()),
            merge_parent_hash: Some("cd".repeat(32)),
        };

        let ingested = try_ingest_merged_main(&repo, &merged).unwrap();
        assert_eq!(ingested.map(|h| h.to_string()), Some(server_head.clone()));
        let commit = repo
            .get_commit(&Hash(server_head.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(commit.message.as_deref(), Some("ship feature"));
        assert_eq!(commit.files.len(), 1);
        assert_eq!(commit.files[0].path, "feature.txt");
        assert_eq!(commit.files[0].change_type, oak_core::ChangeType::Added);
        assert_eq!(
            repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
            Some(server_head)
        );
    }

    fn manifest_entry(repo: &dyn Repository, path: &str, content: &str) -> (ManifestEntry, Hash) {
        let blob_hash = repo.put_blob(content.as_bytes().to_vec()).unwrap();
        (
            ManifestEntry {
                path: path.to_string(),
                blob_hash: blob_hash.clone(),
                mode: oak_core::FileMode::Regular,
            },
            blob_hash,
        )
    }

    #[test]
    fn lca_manifest_errors_without_verified_common_ancestor() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("left".to_string(), None, None))
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                None,
                None,
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();

        let (right_entry, _) = manifest_entry(&repo, "right.txt", "right\n");
        let right_manifest_hash = repo.put_manifest(vec![right_entry]).unwrap();
        let right_head = repo
            .put_commit(
                "right".to_string(),
                None,
                None,
                right_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("right", &right_head).unwrap();

        let err =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&right_head)).unwrap_err();
        assert!(
            matches!(
                err,
                OakError::NoVerifiedCommonAncestor {
                    ref left,
                    ref right
                } if left == "left" && right == "right"
            ),
            "expected clear no-common-ancestor error, got {err}"
        );
    }

    #[test]
    fn lca_manifest_allows_declared_root_base_despite_branch_side_gap() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "left".to_string(),
            None,
            Some("right".to_string()),
        ))
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let missing_parent = Hash("ab".repeat(32));
        let (root_entry, _) = manifest_entry(&repo, "root.txt", "root\n");
        let root_manifest_hash = repo.put_manifest(vec![root_entry]).unwrap();
        let root_head = repo
            .put_commit(
                "left".to_string(),
                None,
                None,
                root_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                Some(root_head),
                Some(missing_parent.clone()),
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();

        let (right_entry, _) = manifest_entry(&repo, "right.txt", "right\n");
        let right_manifest_hash = repo.put_manifest(vec![right_entry]).unwrap();
        let right_head = repo
            .put_commit(
                "right".to_string(),
                None,
                None,
                right_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("right", &right_head).unwrap();

        let manifest =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&right_head)).unwrap();
        assert!(
            manifest.entries.is_empty(),
            "declared root-seeded branch should use the empty repository tree"
        );
    }

    #[test]
    fn lca_manifest_reports_incomplete_ancestry_without_verified_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "left".to_string(),
            None,
            Some("right".to_string()),
        ))
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let missing_parent = Hash("ab".repeat(32));
        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                Some(missing_parent.clone()),
                None,
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();

        let (right_entry, _) = manifest_entry(&repo, "right.txt", "right\n");
        let right_manifest_hash = repo.put_manifest(vec![right_entry]).unwrap();
        let right_head = repo
            .put_commit(
                "right".to_string(),
                None,
                None,
                right_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("right", &right_head).unwrap();

        let err =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&right_head)).unwrap_err();
        assert!(
            matches!(
                err,
                OakError::IncompleteAncestry {
                    ref left,
                    ref right,
                    ref missing
                } if left == "left" && right == "right" && missing.contains(missing_parent.short())
            ),
            "expected incomplete-ancestry error, got {err}"
        );
    }

    #[test]
    fn lca_manifest_reports_incomplete_ancestry_before_stale_common_base() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("left".to_string(), None, None))
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry]).unwrap();
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
        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                Some(base_head.clone()),
                Some(missing_merge_parent.clone()),
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();
        repo.set_branch_head("right", &base_head).unwrap();

        let err =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&base_head)).unwrap_err();
        assert!(
            matches!(
                err,
                OakError::IncompleteAncestry {
                    ref left,
                    ref right,
                    ref missing
                } if left == "left" && right == "right" && missing.contains(missing_merge_parent.short())
            ),
            "expected incomplete-ancestry error instead of stale base, got {err}"
        );
    }

    #[test]
    fn lca_manifest_allows_missing_history_older_than_selected_base() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("left".to_string(), None, None))
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let missing_older_parent = Hash("ef".repeat(32));
        let (base_entry, _) = manifest_entry(&repo, "base.txt", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                Some(missing_older_parent),
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();

        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo
            .put_manifest(vec![base_entry.clone(), left_entry])
            .unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                Some(base_head.clone()),
                None,
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();
        repo.set_branch_head("right", &base_head).unwrap();

        let manifest =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&base_head)).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].path, base_entry.path);
        assert_eq!(manifest.entries[0].blob_hash, base_entry.blob_hash);
        assert_eq!(manifest.entries[0].mode, base_entry.mode);
    }

    #[test]
    fn lca_manifest_allows_declared_root_base_despite_parent_side_gap() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "left".to_string(),
            None,
            Some("right".to_string()),
        ))
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest_hash = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                None,
                None,
                left_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();

        let missing_right_parent = Hash("98".repeat(32));
        let (right_entry, _) = manifest_entry(&repo, "right.txt", "right\n");
        let right_manifest_hash = repo.put_manifest(vec![right_entry]).unwrap();
        let right_head = repo
            .put_commit(
                "right".to_string(),
                Some(missing_right_parent),
                None,
                right_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("right", &right_head).unwrap();

        let manifest =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&right_head)).unwrap();
        assert!(
            manifest.entries.is_empty(),
            "declared root-seeded branch should use the empty repository tree"
        );
    }

    #[test]
    fn lca_manifest_does_not_use_merge_parent_root_as_declared_empty_base() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "left".to_string(),
            None,
            Some("right".to_string()),
        ))
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("right".to_string(), None, None))
            .unwrap();

        let (left_root_entry, _) = manifest_entry(&repo, "left-root.txt", "left root\n");
        let left_root_manifest = repo.put_manifest(vec![left_root_entry]).unwrap();
        let left_root = repo
            .put_commit(
                "left".to_string(),
                None,
                None,
                left_root_manifest,
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

        let (left_entry, _) = manifest_entry(&repo, "left.txt", "left\n");
        let left_manifest = repo.put_manifest(vec![left_entry]).unwrap();
        let left_head = repo
            .put_commit(
                "left".to_string(),
                Some(left_root),
                Some(foreign_root),
                left_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("left", &left_head).unwrap();

        let (right_entry, _) = manifest_entry(&repo, "right.txt", "right\n");
        let right_manifest = repo.put_manifest(vec![right_entry]).unwrap();
        let right_head = repo
            .put_commit(
                "right".to_string(),
                None,
                None,
                right_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("right", &right_head).unwrap();

        let err =
            find_lca_manifest_with_parent(&repo, "left", "right", Some(&right_head)).unwrap_err();
        assert!(
            matches!(
                err,
                OakError::NoVerifiedCommonAncestor {
                    ref left,
                    ref right
                } if left == "left" && right == "right"
            ),
            "merge-parent root must not trigger empty-base fallback, got {err}"
        );
    }

    #[test]
    fn merge_continue_preserves_incomplete_ancestry_when_recomputing_state() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();

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
        repo.set_branch_head("main", &base_head).unwrap();

        let missing_edge = Hash("12".repeat(32));
        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head),
                Some(missing_edge),
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_current_branch("feature").unwrap();
        repo.set_head(&feature_head).unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();

        let err = merge_continue(temp.path()).unwrap_err();
        assert!(
            matches!(err, OakError::IncompleteAncestry { .. }),
            "expected typed incomplete ancestry error, got {err:?}"
        );
    }

    #[test]
    fn recompute_merge_state_fails_when_parent_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();

        let missing_manifest = Hash("44".repeat(32));
        let parent_commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_manifest.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::Utc::now(),
        )
        .unwrap();
        let parent_head = parent_commit.hash.clone();
        repo.store_commit(&parent_commit).unwrap();
        repo.set_branch_head("main", &parent_head).unwrap();

        let (feature_entry, _) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(parent_head),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();

        let err = recompute_merge_state(&repo, "feature", "main").unwrap_err();
        assert!(
            matches!(
                err,
                OakError::IncompleteManifestData {
                    ref left,
                    ref right,
                    ref missing
                } if left == "feature" && right == "main" && missing.contains(missing_manifest.short())
            ),
            "expected typed missing-manifest error, got {err:?}"
        );
    }

    #[test]
    fn merge_continue_fails_closed_when_recorded_merge_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let blob = repo.put_blob(b"work\n".to_vec()).unwrap();
        let manifest_hash = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked.txt".to_string(),
                blob_hash: blob,
                mode: oak_core::FileMode::Regular,
            }])
            .unwrap();
        let head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "work\n").unwrap();

        let missing_merged_manifest = Hash("54".repeat(32));
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();
        std::fs::write(
            temp.path().join(".oak/MERGE_STATE"),
            serde_json::to_string(&MergeState {
                merged_manifest_hash: missing_merged_manifest.to_string(),
                conflict_paths: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        output::begin_capture();
        let err = merge_continue(temp.path())
            .expect_err("missing recorded merge manifest must fail closed");
        let _ = output::end_capture();

        assert!(
            matches!(
                err,
                OakError::IncompleteManifestData {
                    ref left,
                    ref right,
                    ref missing
                } if left == "merge state"
                    && right == "recorded merge"
                    && missing.contains(missing_merged_manifest.short())
            ),
            "expected typed missing-manifest error, got {err:?}"
        );
        assert!(temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(temp.path().join(".oak/MERGE_STATE").exists());
    }

    #[test]
    fn merge_abort_clears_state_when_head_manifest_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let missing_manifest = Hash("55".repeat(32));
        let head_commit = Commit::with_timestamp(
            "feature".to_string(),
            None,
            None,
            missing_manifest,
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::Utc::now(),
        )
        .unwrap();
        let head = head_commit.hash.clone();
        repo.store_commit(&head_commit).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "conflict markers stay\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();
        std::fs::write(
            temp.path().join(".oak/MERGE_STATE"),
            serde_json::to_string(&MergeState {
                merged_manifest_hash: Hash("56".repeat(32)).to_string(),
                conflict_paths: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        output::begin_capture();
        merge_abort(temp.path())
            .expect("abort should clear merge state even if HEAD is incomplete");
        let captured = output::end_capture();

        assert!(
            captured.contains("working directory left unchanged"),
            "expected explicit partial abort message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "conflict markers stay\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_MSG").exists());
        assert!(!temp.path().join(".oak/MERGE_STATE").exists());
    }

    #[test]
    fn merge_abort_clears_state_when_head_commit_row_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let head = Hash("57".repeat(32));
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_current_branch("feature").unwrap();
        repo.set_foreign_keys(false).unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();
        repo.set_foreign_keys(true).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "conflict markers stay\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();

        output::begin_capture();
        merge_abort(temp.path())
            .expect("abort should clear merge state even if HEAD row is missing");
        let captured = output::end_capture();

        assert!(
            captured.contains("working directory left unchanged"),
            "expected honest unchanged-worktree message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "conflict markers stay\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_MSG").exists());
    }

    #[test]
    fn merge_abort_clears_state_when_head_blob_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let missing_blob = Hash("58".repeat(32));
        repo.set_foreign_keys(false).unwrap();
        let head_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked.txt".to_string(),
                blob_hash: missing_blob,
                mode: oak_core::FileMode::Regular,
            }])
            .unwrap();
        repo.set_foreign_keys(true).unwrap();
        let head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                head_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "conflict markers stay\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();

        output::begin_capture();
        merge_abort(temp.path())
            .expect("abort should clear merge state even if HEAD blob is missing");
        let captured = output::end_capture();

        assert!(
            captured.contains("working directory left unchanged"),
            "expected honest unchanged-worktree message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "conflict markers stay\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_MSG").exists());
    }

    #[test]
    fn merge_abort_clears_truncated_merge_head() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let _repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "conflict markers stay\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();

        output::begin_capture();
        merge_abort(temp.path()).expect("abort should clear corrupt merge state");
        let captured = output::end_capture();

        assert!(
            captured.contains("working directory left unchanged"),
            "expected honest unchanged-worktree message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "conflict markers stay\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_MSG").exists());
    }

    #[test]
    fn merge_abort_preserves_untracked_files() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let head_blob = repo.put_blob(b"head\n".to_vec()).unwrap();
        let head_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked.txt".to_string(),
                blob_hash: head_blob,
                mode: oak_core::FileMode::Regular,
            }])
            .unwrap();
        let head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                head_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();

        let merged_tracked_blob = repo.put_blob(b"merged\n".to_vec()).unwrap();
        let merge_added_blob = repo.put_blob(b"from parent\n".to_vec()).unwrap();
        let merged_manifest = repo
            .put_manifest(vec![
                ManifestEntry {
                    path: "tracked.txt".to_string(),
                    blob_hash: merged_tracked_blob,
                    mode: oak_core::FileMode::Regular,
                },
                ManifestEntry {
                    path: "parent-added.txt".to_string(),
                    blob_hash: merge_added_blob,
                    mode: oak_core::FileMode::Regular,
                },
            ])
            .unwrap();

        std::fs::write(temp.path().join("tracked.txt"), "conflicted\n").unwrap();
        std::fs::write(temp.path().join("parent-added.txt"), "from parent\n").unwrap();
        std::fs::write(temp.path().join("scratch.txt"), "keep me\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();
        std::fs::write(
            temp.path().join(".oak/MERGE_STATE"),
            serde_json::to_string(&MergeState {
                merged_manifest_hash: merged_manifest.to_string(),
                conflict_paths: vec!["tracked.txt".to_string()],
            })
            .unwrap(),
        )
        .unwrap();

        output::begin_capture();
        merge_abort(temp.path()).expect("abort should restore HEAD without deleting scratch files");
        let _ = output::end_capture();

        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "head\n"
        );
        assert!(
            !temp.path().join("parent-added.txt").exists(),
            "tracked merge addition should be removed"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("scratch.txt")).unwrap(),
            "keep me\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_STATE").exists());
    }

    #[test]
    fn merge_abort_with_corrupt_merge_state_reports_partial_restore() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

        let head_blob = repo.put_blob(b"head\n".to_vec()).unwrap();
        let head_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "tracked.txt".to_string(),
                blob_hash: head_blob,
                mode: oak_core::FileMode::Regular,
            }])
            .unwrap();
        let head = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                head_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &head).unwrap();
        repo.set_head(&head).unwrap();

        std::fs::write(temp.path().join("tracked.txt"), "conflicted\n").unwrap();
        std::fs::write(
            temp.path().join("parent-added.txt"),
            format!("{OURS_MARKER}\nours\n=======\ntheirs\n{THEIRS_MARKER}\n"),
        )
        .unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_STATE"), "{not json").unwrap();

        output::begin_capture();
        merge_abort(temp.path()).expect("abort should clear corrupt merge state");
        let captured = output::end_capture();

        assert!(
            captured.contains("partially restored"),
            "expected partial restore message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "head\n"
        );
        assert!(
            temp.path().join("parent-added.txt").exists(),
            "without recorded merge state, merge-added paths must be preserved"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_STATE").exists());
    }

    #[test]
    fn merge_abort_without_head_reports_worktree_left_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join(".oak")).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_current_branch("feature").unwrap();

        std::fs::write(temp.path().join("scratch.txt"), "keep me\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_HEAD"), "main\nfeature\n").unwrap();
        std::fs::write(temp.path().join(".oak/MERGE_MSG"), "merge\n").unwrap();

        output::begin_capture();
        merge_abort(temp.path()).expect("headless abort should still clear merge state");
        let captured = output::end_capture();

        assert!(
            captured.contains("working directory left unchanged"),
            "expected honest unchanged-worktree message, got {captured:?}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("scratch.txt")).unwrap(),
            "keep me\n"
        );
        assert!(!temp.path().join(".oak/MERGE_HEAD").exists());
        assert!(!temp.path().join(".oak/MERGE_MSG").exists());
    }

    #[cfg(unix)]
    #[test]
    fn find_conflict_markers_does_not_follow_symlinked_dirs() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(
            outside.path().join("conflicted.txt"),
            format!("{OURS_MARKER} ours\nours\n=======\ntheirs\n{THEIRS_MARKER} theirs\n"),
        )
        .unwrap();
        symlink(outside.path(), temp.path().join("linked-outside")).unwrap();

        let ignore = IgnorePatterns::new(temp.path()).unwrap();
        let conflicted = find_conflict_markers(temp.path(), temp.path(), &ignore).unwrap();
        assert!(
            conflicted.is_empty(),
            "scanner must not follow symlinked dirs outside repo: {conflicted:?}"
        );
    }

    /// Seed `main` (holding README.md) and a current `feature` branch parented
    /// on it (holding README.md + feature.txt). Returns the repo plus the two
    /// heads and feature's manifest so callers can drive the branch-transition
    /// helpers and assert against a known starting point.
    fn seed_main_and_feature(db_dir: &Path) -> (oak_core::SqliteRepository, Hash, Hash, Manifest) {
        let repo = oak_core::SqliteRepository::open(&db_dir.join("oak.db")).unwrap();

        let (readme_entry, readme_blob) = manifest_entry(&repo, "README.md", "base\n");
        let main_manifest_hash = repo.put_manifest(vec![readme_entry.clone()]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                main_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(readme_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
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
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![readme_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(main_head.clone()),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        (repo, main_head, feature_head, feature_manifest)
    }

    // The success path (and a successful already-closed recovery) hands the
    // helper main's fresh head: the new branch seeds there and the worktree is
    // reset to main, dropping feature-only files.
    #[test]
    fn switch_to_fresh_main_branch_seeds_at_main_and_resets_worktree() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let (repo, main_head, _feature_head, feature_manifest) = seed_main_and_feature(temp.path());

        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        crate::commands::reset::reset_to_manifest(&lock, &root, &repo, &feature_manifest).unwrap();
        assert!(root.join("feature.txt").exists());

        switch_to_fresh_main_branch(&lock, &repo, &root, "feature", Some(main_head.clone()))
            .unwrap();

        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_ne!(current, "feature");
        assert_eq!(
            repo.get_branch(&current)
                .unwrap()
                .unwrap()
                .parent_branch
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(main_head.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(main_head));
        assert!(
            repo.get_branch("feature").unwrap().is_none(),
            "retired branch should be removed locally"
        );
        // Worktree was reset to main: the feature-only file is gone.
        assert!(!root.join("feature.txt").exists());
        assert!(root.join("README.md").exists());
    }

    // The fallback path (already-closed recovery that can't prove the squash
    // landed) passes `None`: the new branch seeds at the retired branch's own
    // tip and the worktree is left untouched so nothing is discarded.
    #[test]
    fn switch_to_fresh_main_branch_fallback_seeds_at_branch_tip_without_reset() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let (repo, _main_head, feature_head, feature_manifest) = seed_main_and_feature(temp.path());

        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        crate::commands::reset::reset_to_manifest(&lock, &root, &repo, &feature_manifest).unwrap();
        assert!(root.join("feature.txt").exists());

        switch_to_fresh_main_branch(&lock, &repo, &root, "feature", None).unwrap();

        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_ne!(current, "feature");
        assert_eq!(
            repo.get_branch(&current)
                .unwrap()
                .unwrap()
                .parent_branch
                .as_deref(),
            Some("main")
        );
        // Seeded at feature's own tip, not main.
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(feature_head.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(feature_head));
        assert!(repo.get_branch("feature").unwrap().is_none());
        // Worktree untouched: feature work is preserved on the fresh branch.
        assert!(root.join("feature.txt").exists());
        assert!(root.join("README.md").exists());
    }

    #[test]
    fn reconciles_current_branch_when_main_contains_remote_squash_merge() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
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
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
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
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        crate::commands::reset::reset_to_manifest(&lock, &root, &repo, &feature_manifest).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        assert_eq!(
            crate::commands::commit::unmerged_commit_count(&repo, "feature").unwrap(),
            1,
            "the old status path reports stale unmerged work"
        );

        let plan = plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
            .unwrap()
            .expect("current branch should reconcile after remote merge");
        let reconciled = apply_remote_merge_reconcile(&lock, &repo, &root, true, plan)
            .unwrap()
            .expect("planned reconciliation should rotate branches");

        assert_eq!(reconciled.closed_branch, "feature");
        assert!(reconciled.reset_worktree);
        assert!(
            repo.get_branch("feature").unwrap().is_none(),
            "remotely merged branch should be removed from local history"
        );

        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_eq!(current, reconciled.new_branch);
        assert_ne!(current, "feature");
        assert_eq!(
            repo.get_branch(&current)
                .unwrap()
                .unwrap()
                .parent_branch
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(main_after_web_merge.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(main_after_web_merge));

        let (changes, _, branch_name) = crate::commands::commit::compute_changes(&repo, &root)
            .expect("status computation should still work");
        assert!(changes.is_empty());
        assert_eq!(branch_name.as_deref(), Some(current.as_str()));
    }

    #[test]
    fn closed_current_branch_without_main_merge_proof_does_not_reconcile() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(main_head.clone()),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        assert!(
            plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
                .unwrap()
                .is_none(),
            "closed branch alone is not enough proof of a web merge"
        );
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("feature")
        );
        assert_eq!(repo.get_head().unwrap(), Some(feature_head));
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Closed
        );
    }

    #[test]
    fn closed_current_branch_with_main_merge_proof_does_not_reconcile_again() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
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
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head.clone()),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        assert!(
            plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
                .unwrap()
                .is_none(),
            "switching back to a closed merged branch should not mint another branch"
        );
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("feature")
        );
        assert_eq!(repo.get_head().unwrap(), Some(feature_head));
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Closed
        );
    }

    #[test]
    fn closed_current_branch_with_main_merge_proof_reconciles_when_open_at_pull_start() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
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
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head.clone()),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        let plan = plan_remote_merge_reconcile(
            &repo,
            RemoteMergeReconcileScope::Pull {
                branch_open_at_start: Some("feature"),
            },
        )
        .unwrap()
        .expect("pull should reconcile a branch that was open before server marked it closed");
        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        let reconciled = apply_remote_merge_reconcile(&lock, &repo, &root, true, plan)
            .unwrap()
            .expect("planned pull reconciliation should rotate branches");

        assert_eq!(reconciled.closed_branch, "feature");
        assert!(reconciled.reset_worktree);
        assert!(
            repo.get_branch("feature").unwrap().is_none(),
            "remotely merged branch should be removed from local history"
        );
        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_eq!(current, reconciled.new_branch);
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(main_after_web_merge.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(main_after_web_merge));
    }

    /// The old `create_conflict_content` wrapped the entire file in one set
    /// of markers, so even a one-line divergence produced a ~50-line
    /// conflict. With diffy's 3-way merge, identical surrounding lines come
    /// through clean and only the diverging hunk is marked.
    #[test]
    fn conflict_content_marks_only_diverging_hunks() {
        let base = "alpha\nshared line\nomega\n";
        let ours = "alpha\nour change\nomega\n";
        let theirs = "alpha\ntheir change\nomega\n";

        let out = create_conflict_content(base, ours, theirs, "feature", "main");

        // Surrounding unchanged lines stay outside the markers.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.first().copied(), Some("alpha"));
        assert_eq!(lines.last().copied(), Some("omega"));

        // The marker labels are rewritten to the branch names.
        let feature_marker = format!("{OURS_MARKER} feature");
        let main_marker = format!("{THEIRS_MARKER} main");
        assert!(out.contains(&feature_marker), "actual: {out}");
        assert!(out.contains(&main_marker), "actual: {out}");

        // The unchanged lines are NOT inside the conflict block.
        let inside_conflict = out
            .split(&feature_marker)
            .nth(1)
            .and_then(|s| s.split(&main_marker).next())
            .unwrap_or("");
        assert!(
            !inside_conflict.contains("alpha"),
            "shared `alpha` leaked into conflict block: {inside_conflict}"
        );
        assert!(
            !inside_conflict.contains("omega"),
            "shared `omega` leaked into conflict block: {inside_conflict}"
        );
        assert!(inside_conflict.contains("our change"));
        assert!(inside_conflict.contains("their change"));
    }

    /// When both sides made the exact same change relative to base, diffy
    /// returns a clean merge — no markers in the output.
    #[test]
    fn conflict_content_clean_when_both_sides_agree() {
        let base = "alpha\nbeta\ngamma\n";
        let same = "alpha\nbeta changed\ngamma\n";

        let out = create_conflict_content(base, same, same, "feature", "main");
        assert!(!out.contains(OURS_MARKER), "expected clean merge: {out}");
        assert!(!out.contains(THEIRS_MARKER), "expected clean merge: {out}");
        assert_eq!(out.trim_end(), "alpha\nbeta changed\ngamma");
    }

    /// The regression this guards: both sides edit the *same file* but at
    /// distant, non-overlapping lines. diffy reconciles them into one clean
    /// file with no markers, so `content_has_conflict_markers` is false and the
    /// merge/sync/mount-pull call sites must NOT report a conflict. Before the
    /// fix, those sites flagged any both-sides-edited file regardless of
    /// overlap.
    #[test]
    fn conflict_content_clean_when_edits_dont_overlap() {
        let base = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
        let ours = "ALPHA-CHANGED\nbeta\ngamma\ndelta\nepsilon\n";
        let theirs = "alpha\nbeta\ngamma\ndelta\nEPSILON-CHANGED\n";

        let out = create_conflict_content(base, ours, theirs, "feature", "main");

        assert!(
            !content_has_conflict_markers(&out),
            "non-overlapping edits must merge clean, got:\n{out}"
        );
        // Both sides' changes survive in the merged result.
        assert!(out.contains("ALPHA-CHANGED"));
        assert!(out.contains("EPSILON-CHANGED"));
    }

    /// A real unresolved conflict has both markers at column 0 — flag it.
    #[test]
    fn conflict_markers_detected_at_column_zero() {
        let content = format!(
            "alpha\n{OURS_MARKER} feature\nour change\n=======\ntheir change\n{THEIRS_MARKER} main\nomega\n"
        );
        assert!(content_has_conflict_markers(&content));
    }

    /// Markers that appear only inside string literals / mid-line — as they do
    /// in oak's own source (doc comments, test fixtures, error messages) — are
    /// NOT real conflicts and must not be flagged.
    #[test]
    fn conflict_markers_ignored_when_not_at_column_zero() {
        // Indented (mid-line) markers, e.g. inside a string literal in code.
        let indented = format!("    let s = \"{OURS_MARKER} x\";\n    // {THEIRS_MARKER} y\n");
        assert!(!content_has_conflict_markers(&indented));

        // Both markers present but embedded inside a single source line.
        let embedded =
            format!("remove every {OURS_MARKER} / ======= / {THEIRS_MARKER} line before saving\n");
        assert!(!content_has_conflict_markers(&embedded));
    }

    /// Only one marker at column 0 (without its pair) is not a conflict.
    #[test]
    fn conflict_markers_require_both_sides() {
        let only_ours = format!("{OURS_MARKER} feature\nour change\n");
        assert!(!content_has_conflict_markers(&only_ours));
    }

    /// Continue-time checks reject any column-zero delimiter, including
    /// malformed one-sided marker fragments.
    #[test]
    fn conflict_marker_delimiters_detect_stray_column_zero_markers() {
        assert!(content_has_conflict_marker_delimiter(&format!(
            "{OURS_MARKER} feature\nresolved\n"
        )));
        assert!(content_has_conflict_marker_delimiter("resolved\n=======\n"));
        assert!(content_has_conflict_marker_delimiter(&format!(
            "resolved\n{THEIRS_MARKER} main\n"
        )));
        assert!(!content_has_conflict_marker_delimiter(&format!(
            "    {OURS_MARKER} in a string\n    =======\n    {THEIRS_MARKER} in a string\n"
        )));
    }

    /// `prune_merged_branches` removes exactly the branches whose tips main's
    /// history records as squash-merged (plus headless closed rows), and
    /// keeps everything else: the current branch, branches with unmerged
    /// work, and fresh branches seeded at main's head.
    #[test]
    fn prune_removes_merged_branches_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, _) = manifest_entry(&repo, "README.md", "base\n");
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
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let new_branch = |name: &str| {
            repo.store_branch(&oak_core::Branch::new(
                name.to_string(),
                None,
                Some("main".to_string()),
            ))
            .unwrap();
        };
        let commit_on = |name: &str, file: &str, parent: &Hash| {
            let (entry, _) = manifest_entry(&repo, file, file);
            let manifest_hash = repo.put_manifest(vec![base_entry.clone(), entry]).unwrap();
            repo.put_commit(
                name.to_string(),
                Some(parent.clone()),
                None,
                manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap()
        };

        // Merged on main (web UI / another client): tip recorded as the
        // squash commit's merge_parent_hash.
        new_branch("merged");
        let merged_tip = commit_on("merged", "merged.txt", &base_head);
        repo.set_branch_head("merged", &merged_tip).unwrap();
        let squash = repo
            .put_commit(
                "main".to_string(),
                Some(base_head.clone()),
                Some(merged_tip.clone()),
                repo.get_commit(&merged_tip).unwrap().unwrap().manifest_hash,
                "<remote>".to_string(),
                Some("merged".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &squash).unwrap();

        // Merged, but with a local commit past the merged tip — must be kept.
        new_branch("merged-plus-work");
        repo.set_branch_head("merged-plus-work", &merged_tip)
            .unwrap();
        let extra = commit_on("merged-plus-work", "extra.txt", &merged_tip);
        repo.set_branch_head("merged-plus-work", &extra).unwrap();

        // Fresh branch seeded at main's new head: head is a main commit, not
        // a merged tip — must be kept.
        new_branch("fresh");
        repo.set_branch_head("fresh", &squash).unwrap();

        // Closed tombstone without a head: nothing to lose — pruned.
        let mut headless =
            oak_core::Branch::new("headless".to_string(), None, Some("main".to_string()));
        headless.status = BranchStatus::Closed;
        repo.store_branch(&headless).unwrap();

        // Current branch is never pruned, even at the merged tip.
        new_branch("current");
        repo.set_branch_head("current", &merged_tip).unwrap();
        repo.set_current_branch("current").unwrap();

        let mut pruned = prune_merged_branches(&repo).unwrap();
        pruned.sort();
        assert_eq!(pruned, vec!["headless".to_string(), "merged".to_string()]);

        assert!(repo.get_branch("merged").unwrap().is_none());
        assert!(repo.get_branch("headless").unwrap().is_none());
        assert!(repo.get_branch("merged-plus-work").unwrap().is_some());
        assert!(repo.get_branch("fresh").unwrap().is_some());
        assert!(repo.get_branch("current").unwrap().is_some());
        assert!(repo.get_branch("main").unwrap().is_some());
        assert!(
            repo.get_branch_head("merged").unwrap().is_none(),
            "pruned branch's head pointer should be deleted too"
        );
    }
}
