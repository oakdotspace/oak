use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use dialoguer::{Confirm, Input, Select};
use oak_core::{hash_bytes, Blob, Hash, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use serde::Deserialize;

use super::merge_check::{self, MergeCheckJson, MergeCheckTarget};
use crate::output;
use crate::transfer::{self, ChunkBusyRetryState, ChunkUploader, TransferCounts, TransferSession};

pub const DEFAULT_REMOTE: &str = "https://oak.space";
pub const PUSH_REPO_PLACEHOLDER_COMMAND: &str = "oak push --repo <org>/<repo>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushRemoteSource {
    Explicit,
    Env,
    Stored,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPushRemote {
    pub(crate) url: String,
    pub(crate) persist: bool,
    pub(crate) source: PushRemoteSource,
}

pub fn normalize_remote_url(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    if remote.is_empty() {
        return None;
    }
    let mut url = reqwest::Url::parse(remote).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    // Remote identity and every public rendering are origin-like, never a
    // credential container. Bearer credentials have their own store; URL
    // userinfo, query strings and fragments must not reach receipts, JSON or
    // logs (and are not meaningful parts of an Oak API base URL).
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.as_str().trim_end_matches('/').to_string())
}

pub fn env_remote_override() -> Option<String> {
    std::env::var("OAK_REMOTE")
        .ok()
        .and_then(|remote| normalize_remote_url(&remote))
}

// Wire protocol types live in `oak_core::protocol` (the single source of truth
// shared by the CLI and the hosted server). They're aliased back
// to the names this module has always used so the call sites below are
// unchanged. `BranchPushData`'s wire shape is identical to the old local
// `BranchData`, and `ChunkCheckServerResponse` to the old `ChunkCheckResponse`.
use oak_core::protocol::{
    BlobCheckRequest, BlobCheckResponse, BlobData, BlobProofChunk, BlobProofDescriptor,
    BlobProofFinalizeRequest, BlobProofMappingPage, BlobProofPagesRequest, BlobProofPagesResponse,
    BlobProofRequest, BlobProofResponse, BranchPushData as BranchData, ChunkRefData as ChunkRef,
    CommitData, CommitInfoRequest, CommitInfoResponse, ErrorResponse, FileChangeData,
    MappingProofJob, PushRequest, PushResponse, StagedAbortRequest, StagedPushRequest, TreeData,
};

/// Minimal client-side view of `GET /api/{owner}/{name}` — the push flow only
/// needs the repo head, so it deserializes leniently rather than pulling in the
/// full `protocol::RepoResponse`.
#[derive(Deserialize)]
struct RepoResponse {
    head: Option<String>,
}

/// Subset of the per-branch GET response we need to compute `remote_head`
/// for a branch-scoped push.
#[derive(Deserialize)]
struct BranchHeadResponse {
    head: Option<String>,
}

fn commit_to_wire(commit: &oak_core::Commit) -> CommitData {
    CommitData {
        hash: commit.hash.to_string(),
        branch_name: commit.branch_name.clone(),
        parent_hash: commit.parent_hash.as_ref().map(|h| h.to_string()),
        merge_parent_hash: commit.merge_parent_hash.as_ref().map(|h| h.to_string()),
        manifest_hash: commit.manifest_hash.to_string(),
        author: commit.author.clone(),
        message: commit.message.clone(),
        timestamp: commit.timestamp.to_rfc3339(),
        files: commit
            .files
            .iter()
            .map(|file| FileChangeData {
                path: file.path.clone(),
                change_type: match file.change_type {
                    oak_core::ChangeType::Added => "added".to_string(),
                    oak_core::ChangeType::Modified => "modified".to_string(),
                    oak_core::ChangeType::Deleted => "deleted".to_string(),
                    oak_core::ChangeType::Renamed => "renamed".to_string(),
                },
                old_blob_hash: file.old_blob_hash.as_ref().map(|h| h.to_string()),
                new_blob_hash: file.new_blob_hash.as_ref().map(|h| h.to_string()),
                old_path: file.old_path.clone(),
                old_mode: file
                    .old_mode
                    .map(|mode| oak_core::protocol::file_mode_to_wire(mode).to_string()),
                new_mode: file
                    .new_mode
                    .map(|mode| oak_core::protocol::file_mode_to_wire(mode).to_string()),
            })
            .collect(),
    }
}

/// Verify the exact DTOs assembled by the outgoing path before any blob/chunk
/// probe, upload, repository creation, or final push. Read-only remote head
/// discovery happens first so already-owned history is not rescanned or made
/// capable of blocking an unrelated incremental push.
fn admit_outgoing_wire_objects(commits: &[CommitData], trees: &[TreeData]) -> Result<()> {
    for commit in commits {
        oak_core::protocol::commit_data_to_core(commit).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing commit {}: {error}; no remote state was mutated",
                commit.hash
            ))
        })?;
    }
    for tree in trees {
        oak_core::protocol::tree_data_to_core(tree).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing tree {}: {error}; no remote state was mutated",
                tree.hash
            ))
        })?;
    }
    Ok(())
}

fn admit_local_blob(blob: &Blob, expected_hash: &Hash) -> Result<()> {
    let actual_size = blob.content.len() as u64;
    if blob.size != actual_size {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {expected_hash}: stored size {} does not match decoded content size {actual_size}; no remote state was mutated",
            blob.size
        )));
    }
    let actual_hash = hash_bytes(&blob.content);
    if blob.hash != *expected_hash || actual_hash != *expected_hash {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {expected_hash}: content hash mismatch (decoded bytes hash to {actual_hash}); no remote state was mutated"
        )));
    }
    Ok(())
}

fn admit_local_blob_descriptor(
    repo: &SqliteRepository,
    expected_hash: &Hash,
) -> Result<Option<PlannedBlob>> {
    if *expected_hash == Blob::empty_hash() {
        oak_core::ensure_empty_blob(repo, expected_hash)?;
    }
    let Some(size) = repo.get_blob_size(expected_hash)? else {
        return Ok(None);
    };
    if let Some(mut chunks) = repo.get_blob_chunks(expected_hash)? {
        if !chunks.is_empty() {
            chunks.sort_by_key(|chunk| chunk.offset);
            let mut expected_offset = 0u64;
            let mut hasher = blake3::Hasher::new();
            let chunk_refs = chunks.len();
            for chunk in chunks {
                if chunk.offset != expected_offset {
                    return Err(OakError::InvalidArgument(format!(
                        "push admission phase rejected local blob {expected_hash}: chunk mapping starts at {}, expected {expected_offset}; no remote state was mutated",
                        chunk.offset
                    )));
                }
                let bytes = repo.get_chunk(&chunk.hash)?.ok_or_else(|| {
                    OakError::InvalidArgument(format!(
                        "push admission phase rejected local blob {expected_hash}: chunk {} is missing; no remote state was mutated",
                        chunk.hash
                    ))
                })?;
                if bytes.len() != chunk.length as usize || hash_bytes(&bytes) != chunk.hash {
                    return Err(OakError::InvalidArgument(format!(
                        "push admission phase rejected local blob {expected_hash}: chunk {} fails size/hash verification; no remote state was mutated",
                        chunk.hash
                    )));
                }
                hasher.update(&bytes);
                expected_offset = expected_offset
                    .checked_add(chunk.length as u64)
                    .ok_or_else(|| {
                        OakError::InvalidArgument(format!(
                            "push admission phase rejected local blob {expected_hash}: chunk offsets overflow; no remote state was mutated"
                        ))
                    })?;
            }
            let actual = Hash(hasher.finalize().to_hex().to_string());
            if expected_offset != size || &actual != expected_hash {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected local blob {expected_hash}: streamed chunk chain is {expected_offset} bytes and hashes to {actual}, expected {size} bytes; no remote state was mutated"
                )));
            }
            return Ok(Some(PlannedBlob {
                hash: expected_hash.clone(),
                size,
                chunk_refs,
            }));
        }
    }
    if transfer::is_content_defined(size) {
        // Stream and hash; a large blob is never held whole.
        verify_local_blob_stream(repo, expected_hash, size)?;
    } else {
        let blob = repo.get_blob(expected_hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected local blob {expected_hash}: metadata exists but bytes are missing; no remote state was mutated"
            ))
        })?;
        admit_local_blob(&blob, expected_hash)?;
    }
    Ok(Some(PlannedBlob {
        hash: expected_hash.clone(),
        size,
        chunk_refs: 0,
    }))
}

pub(super) fn admit_commit_file_changes(
    repo: &SqliteRepository,
    commit: &oak_core::Commit,
) -> Result<()> {
    // Git imports intentionally omit per-commit file records. Their tree is
    // still authoritative and admitted separately; a non-empty file list,
    // however, must close over the parent/current manifests it claims.
    if commit.files.is_empty() {
        return Ok(());
    }
    let fail = |detail: String| {
        OakError::InvalidArgument(format!(
            "push admission phase rejected outgoing commit {}: file-change closure {detail}; no remote state was mutated",
            commit.hash
        ))
    };
    let parent = match &commit.parent_hash {
        Some(parent_hash) => {
            let parent_commit = repo.get_commit(parent_hash)?.ok_or_else(|| {
                fail(format!(
                    "cannot validate old-side references because parent {parent_hash} is not materialized locally"
                ))
            })?;
            repo.get_manifest(&parent_commit.manifest_hash)
                .map_err(|error| {
                    OakError::InvalidArgument(format!(
                        "push admission phase rejected local tree {}: {error}; no remote state was mutated",
                        parent_commit.manifest_hash
                    ))
                })?
                .ok_or_else(|| {
                    fail(format!(
                        "parent manifest {} is not materialized locally",
                        parent_commit.manifest_hash
                    ))
                })?
        }
        None => oak_core::Manifest::empty(),
    };
    let current = repo
        .get_manifest(&commit.manifest_hash)
        .map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected local tree {}: {error}; no remote state was mutated",
                commit.manifest_hash
            ))
        })?
        .ok_or_else(|| {
            fail(format!(
                "current manifest {} is not materialized locally",
                commit.manifest_hash
            ))
        })?;
    oak_core::validate_manifest_transition(&parent, &current, &commit.files).map_err(fail)
}

fn plan_outgoing_commits(
    repo: &SqliteRepository,
    branch_name: &str,
    tip: Option<&Hash>,
    required_boundary: Option<&Hash>,
    allowed_external: &std::collections::HashSet<String>,
) -> Result<Vec<oak_core::Commit>> {
    let Some(tip) = tip else {
        return Ok(Vec::new());
    };
    if required_boundary == Some(tip) {
        return Ok(Vec::new());
    }

    // Iterative post-order DFS produces parent-before-child wire order without
    // trusting imported timestamps or risking stack overflow on deep history.
    // Remote heads are opaque proven boundaries: old local ancestry is never
    // walked or charged to the outgoing operation.
    let mut state: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let mut loaded: std::collections::HashMap<String, oak_core::Commit> =
        std::collections::HashMap::new();
    let mut ordered = Vec::new();
    let mut reached_boundary = required_boundary.is_none();
    let mut stack = vec![(tip.clone(), false)];
    while let Some((hash, expanded)) = stack.pop() {
        if required_boundary == Some(&hash) {
            reached_boundary = true;
            continue;
        }
        if allowed_external.contains(hash.as_str()) {
            continue;
        }
        if expanded {
            state.insert(hash.to_string(), 2);
            let commit = loaded.remove(hash.as_str()).expect("loaded during DFS");
            admit_commit_file_changes(repo, &commit)?;
            ordered.push(commit);
            continue;
        }
        match state.get(hash.as_str()) {
            Some(1) => {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected outgoing graph: cycle at commit {hash}; no remote state was mutated"
                )));
            }
            Some(2) => continue,
            _ => {}
        }
        let commit = repo.get_commit(&hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing graph: missing parent or merge parent {hash}; no remote state was mutated"
            ))
        })?;
        if commit.branch_name != branch_name {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing {branch_name} graph: dependency {hash} belongs to branch {}; no remote state was mutated",
                commit.branch_name
            )));
        }
        state.insert(hash.to_string(), 1);
        loaded.insert(hash.to_string(), commit.clone());
        stack.push((hash, true));
        if let Some(merge_parent) = commit.merge_parent_hash {
            stack.push((merge_parent, false));
        }
        if let Some(parent) = commit.parent_hash {
            stack.push((parent, false));
        }
    }
    if !reached_boundary {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected outgoing graph: target tip {tip} does not close over required remote boundary {}; no remote state was mutated",
            required_boundary.unwrap()
        )));
    }
    Ok(ordered)
}

fn collect_unproven_external_edges(
    repo: &SqliteRepository,
    branch_name: &str,
    tip: Option<&Hash>,
    boundary: Option<&Hash>,
    allowed_external: &std::collections::HashSet<String>,
) -> Result<std::collections::HashSet<Hash>> {
    let mut external = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<Hash> = tip.into_iter().cloned().collect();
    while let Some(hash) = stack.pop() {
        if boundary == Some(&hash)
            || allowed_external.contains(hash.as_str())
            || !seen.insert(hash.to_string())
        {
            continue;
        }
        let Some(commit) = repo.get_commit(&hash)? else {
            external.insert(hash);
            continue;
        };
        if commit.branch_name != branch_name {
            external.insert(hash);
            continue;
        }
        if let Some(parent) = commit.parent_hash {
            stack.push(parent);
        }
        if let Some(merge_parent) = commit.merge_parent_hash {
            stack.push(merge_parent);
        }
    }
    Ok(external)
}

async fn prove_external_edges(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    hashes: &std::collections::HashSet<Hash>,
) -> Result<()> {
    const MAX_EXTERNAL_EDGE_PROOFS: usize = 256;
    if hashes.is_empty() {
        return Ok(());
    }
    if hashes.len() > MAX_EXTERNAL_EDGE_PROOFS {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase found {} external graph edges (limit {MAX_EXTERNAL_EDGE_PROOFS}); no remote state was mutated",
            hashes.len()
        )));
    }
    let mut requested: Vec<String> = hashes.iter().map(ToString::to_string).collect();
    requested.sort();
    let request = with_auth(
        client
            .post(format!("{remote}/api/{endpoint_path}/commits/info"))
            .json(&CommitInfoRequest {
                hashes: requested.clone(),
                metadata_only: true,
            }),
        api_key,
    );
    let response = crate::http::send_idempotent_with_retry_until(
        request,
        "remote commit-edge proof",
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
    )
    .await?;
    if !response.status().is_success() {
        if matches!(response.status().as_u16(), 404 | 405) {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase requires /commits/info to prove remote commit edges, but this server returned HTTP {}; upgrade the server and retry; no remote state was mutated",
                response.status()
            )));
        }
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase was not authorized to prove older remote commit edges (HTTP {}); refresh credentials and retry; no remote state was mutated",
                response.status()
            )));
        }
        return Err(OakError::InvalidArgument(format!(
            "push admission phase could not prove older remote commit edges (HTTP {}); retry after the server is healthy; no remote state was mutated",
            response.status()
        )));
    }
    let response: CommitInfoResponse = response
        .json()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    validate_external_edge_proofs(hashes, response.commits)
}

fn validate_external_edge_proofs(
    requested: &std::collections::HashSet<Hash>,
    commits: Vec<CommitData>,
) -> Result<()> {
    let mut proven = std::collections::HashSet::new();
    for commit in commits {
        let commit = oak_core::protocol::commit_data_to_core(&commit).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase received an invalid remote commit proof: {error}; no remote state was mutated"
            ))
        })?;
        if !requested.contains(&commit.hash) {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase received unrequested remote commit proof {}; no remote state was mutated",
                commit.hash
            )));
        }
        if !proven.insert(commit.hash.to_string()) {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase received duplicate remote commit proof {}; no remote state was mutated",
                commit.hash
            )));
        }
    }
    let mut expected: Vec<String> = requested.iter().map(ToString::to_string).collect();
    expected.sort();
    let missing: Vec<&String> = expected
        .iter()
        .filter(|hash| !proven.contains(hash.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase could not prove older remote commit edge(s) {}; pull full history before retrying; no remote state was mutated",
            missing.into_iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

/// What the two boundary GETs of a push observed.
struct ObservedPushBoundaries {
    /// The repo GET answered 404: the first publication must create it.
    repo_needs_creation: bool,
    boundaries: PushBoundaries,
}

/// Resolve the server boundaries a push plans against: the repo head and,
/// for a branch-scoped push, the branch head, fetched concurrently. Both
/// requests are read-only. Shared by push and `push --plan`.
async fn fetch_push_boundaries(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    api_key: Option<&str>,
) -> Result<ObservedPushBoundaries> {
    output::vlog(&format!(
        "push: GET {remote}/api/{endpoint_path} (fetch remote head{})",
        if branch_name.is_some() {
            " + branch head, concurrent"
        } else {
            ""
        }
    ));
    let t0 = std::time::Instant::now();
    let repo_head_fut =
        with_auth(client.get(format!("{remote}/api/{endpoint_path}")), api_key).send();
    let branch_head_fut = async {
        match branch_name {
            Some(name) => {
                let branch = super::branch_api_segment(name);
                Some(
                    with_auth(
                        client.get(format!("{remote}/api/{endpoint_path}/branches/{branch}")),
                        api_key,
                    )
                    .send()
                    .await,
                )
            }
            None => None,
        }
    };
    let (resp, branch_head_resp) = tokio::join!(repo_head_fut, branch_head_fut);
    let resp = resp.map_err(|e| OakError::Http(e.to_string()))?;
    output::vlog(&format!(
        "push: GET head returned {} in {:.3}s",
        resp.status(),
        t0.elapsed().as_secs_f64()
    ));

    // Repo-wide head (server's `main` head). For branch-scoped pushes this
    // is the *wrong* "since" point — `default` and `main` are independent
    // chains, so using the repo head would treat every commit on the
    // pushed branch as new and re-collect tree objects for commits the
    // server already has. We only use the repo GET here to confirm the
    // repo exists (and to auto-create it on 404); the actual
    // `remote_head` for a branch push is fetched from the per-branch
    // endpoint below. A 404 is remembered rather than immediately creating
    // the repo: creation is a persistent mutation and must wait until the
    // exact outgoing commit/tree DTOs pass local admission.
    let mut repo_needs_creation = false;
    let repo_head: Option<Hash> = if resp.status().is_success() {
        let repo_info: RepoResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        repo_info.head.map(Hash)
    } else if resp.status().as_u16() == 404 {
        repo_needs_creation = true;
        None
    } else {
        return Err(crate::http::server_error(resp).await);
    };

    // Resolve the branch boundary before planning any mutation. Bootstrap
    // admission needs both the target branch boundary and main's boundary so
    // it can certify the whole operation, not merely the first batch.
    let remote_head: Option<Hash> = if let Some(resp) = branch_head_resp {
        let resp = resp.map_err(|e| OakError::Http(e.to_string()))?;
        if resp.status().is_success() {
            let body: BranchHeadResponse = resp
                .json()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
            body.head.map(Hash)
        } else if resp.status().as_u16() == 404 {
            None
        } else {
            return Err(crate::http::server_error(resp).await);
        }
    } else {
        repo_head.clone()
    };
    Ok(ObservedPushBoundaries {
        repo_needs_creation,
        boundaries: PushBoundaries {
            repo_head,
            remote_head,
        },
    })
}
/// The server heads a branch push plans against: the repo head (server
/// `main`) and the pushed branch's own head.
#[derive(Clone, Debug)]
struct PushBoundaries {
    repo_head: Option<Hash>,
    remote_head: Option<Hash>,
}

/// With an empty server repo (no repo head), a branch parented on `main`
/// whose local `main` has commits first bootstraps `main`. Shared by push
/// and `push --plan`.
fn push_needs_main_bootstrap(repo: &SqliteRepository, branch_name: &str) -> Result<bool> {
    let on_main_child = branch_name != "main"
        && repo
            .get_branch(branch_name)?
            .and_then(|branch| branch.parent_branch)
            .is_some_and(|parent| parent == "main");
    Ok(on_main_child && repo.get_branch_head("main")?.is_some())
}

/// Whether the local target no longer extends the server's head. For a
/// branch push this is ancestry, not mere presence; the repo-scoped fallback
/// keeps the legacy presence check. Shared by push and `push --plan`.
fn push_target_diverged(
    repo: &SqliteRepository,
    branch_name: Option<&str>,
    remote_head: &Hash,
) -> Result<bool> {
    Ok(match branch_name {
        Some(name) => match repo.get_branch_head(name)? {
            Some(lh) => {
                lh != *remote_head && !super::sync::local_history_contains(repo, &lh, remote_head)?
            }
            None => false,
        },
        None => repo.get_head()?.is_some() && !repo.has_commit(remote_head)?,
    })
}

/// The exact outgoing commit suffix an ordinary (non-bootstrap, not
/// pre-planned) branch push admits over the server boundaries. Shared by
/// push and `push --plan` so the plan cannot drift from what push sends.
#[allow(clippy::too_many_arguments)]
async fn plan_ordinary_branch_commits(
    repo: &SqliteRepository,
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    branch_name: &str,
    boundaries: PushBoundaries,
    bootstrap_head: Option<&Hash>,
    force: bool,
) -> Result<Vec<oak_core::Commit>> {
    let allowed_external: std::collections::HashSet<String> = [
        boundaries.repo_head.as_ref(),
        boundaries.remote_head.as_ref(),
        bootstrap_head,
    ]
    .into_iter()
    .flatten()
    .map(ToString::to_string)
    .collect();
    let required_boundary = (!force)
        .then_some(boundaries.remote_head.as_ref())
        .flatten();
    plan_target_with_remote_proofs(
        repo,
        client,
        remote,
        endpoint_path,
        api_key,
        branch_name,
        repo.get_branch_head(branch_name)?.as_ref(),
        required_boundary,
        allowed_external,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn plan_target_with_remote_proofs(
    repo: &SqliteRepository,
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    branch_name: &str,
    tip: Option<&Hash>,
    boundary: Option<&Hash>,
    mut allowed_external: std::collections::HashSet<String>,
) -> Result<Vec<oak_core::Commit>> {
    let external =
        collect_unproven_external_edges(repo, branch_name, tip, boundary, &allowed_external)?;
    prove_external_edges(client, remote, endpoint_path, api_key, &external).await?;
    allowed_external.extend(external.into_iter().map(|hash| hash.to_string()));
    plan_outgoing_commits(repo, branch_name, tip, boundary, &allowed_external)
}

struct PlannedObjects {
    tree_hashes: Vec<Hash>,
    blobs: Vec<PlannedBlob>,
    missing_blobs: std::collections::HashSet<Hash>,
    manifest_entries: std::collections::HashMap<Hash, usize>,
    validation_entries: std::collections::HashMap<Hash, usize>,
    manifest_path_bytes: std::collections::HashMap<Hash, usize>,
    direct_tree_entries: usize,
    tree_metadata_bytes: usize,
    expanded_path_bytes: usize,
    /// Planned blobs the server provably holds (see [`ServerKnownObjects`]);
    /// transport selection need not ask the server about them.
    server_has_blobs: std::collections::HashSet<Hash>,
}

#[derive(Clone, Debug)]
struct PlannedBlob {
    hash: Hash,
    size: u64,
    chunk_refs: usize,
}

#[cfg(test)]
fn wire_size<T: serde::Serialize>(value: &T) -> Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| OakError::InvalidArgument(format!(
            "push admission phase could not size the staged metadata envelope: {error}; no remote state was mutated"
        )))
}

#[derive(Debug, Default, Clone, Copy)]
struct PushOperationTotals {
    commits: usize,
    trees: usize,
    direct_tree_entries: usize,
    resolved_manifest_entries: usize,
    window_resolved_entries: usize,
    file_changes: usize,
    canonical_metadata_bytes: usize,
    expanded_path_bytes: usize,
    chunk_refs: usize,
    blobs: usize,
    declared_blob_bytes: u64,
}

fn push_operation_totals(
    commits: &[&oak_core::Commit],
    objects: &PlannedObjects,
    branches: &[BranchData],
) -> PushOperationTotals {
    let commit_metadata = commits.iter().fold(0usize, |total, commit| {
        total.saturating_add(oak_core::protocol::staged_commit_metadata_bytes(
            &commit_to_wire(commit),
        ))
    });
    let blob_metadata = objects.blobs.iter().fold(0usize, |total, blob| {
        total.saturating_add(blob_metadata_bytes(blob))
    });
    let distinct_roots: std::collections::HashSet<&Hash> =
        commits.iter().map(|commit| &commit.manifest_hash).collect();
    let resolved_manifest_entries = distinct_roots.iter().fold(0usize, |total, root| {
        total.saturating_add(objects.manifest_entries.get(*root).copied().unwrap_or(0))
    });
    PushOperationTotals {
        commits: commits.len(),
        trees: objects.tree_hashes.len(),
        direct_tree_entries: objects.direct_tree_entries,
        resolved_manifest_entries,
        window_resolved_entries: distinct_roots.iter().fold(0usize, |total, root| {
            total.saturating_add(
                objects
                    .validation_entries
                    .get(*root)
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        }),
        file_changes: commits.iter().fold(0usize, |total, commit| {
            total.saturating_add(commit.files.len())
        }),
        canonical_metadata_bytes: branches
            .iter()
            .fold(0usize, |total, branch| {
                total.saturating_add(oak_core::protocol::staged_branch_metadata_bytes(branch))
            })
            .saturating_add(commit_metadata)
            .saturating_add(objects.tree_metadata_bytes)
            .saturating_add(blob_metadata),
        expanded_path_bytes: objects.expanded_path_bytes,
        chunk_refs: objects
            .blobs
            .iter()
            .fold(0usize, |total, blob| total.saturating_add(blob.chunk_refs)),
        blobs: objects.blobs.len(),
        declared_blob_bytes: objects
            .blobs
            .iter()
            .fold(0u64, |total, blob| total.saturating_add(blob.size)),
    }
}

fn blob_metadata_bytes(blob: &PlannedBlob) -> usize {
    blob.hash
        .as_str()
        .len()
        .saturating_add(16)
        .saturating_add(blob.chunk_refs.saturating_mul(64 + 16))
}

fn totals_for_remote_missing_blobs(
    mut totals: PushOperationTotals,
    objects: &PlannedObjects,
    missing: &std::collections::HashSet<Hash>,
) -> PushOperationTotals {
    let all_blob_metadata = objects.blobs.iter().fold(0usize, |total, blob| {
        total.saturating_add(blob_metadata_bytes(blob))
    });
    let mut missing_blob_metadata = 0usize;
    totals.blobs = 0;
    totals.chunk_refs = 0;
    totals.declared_blob_bytes = 0;
    for blob in &objects.blobs {
        if missing.contains(&blob.hash) {
            totals.blobs = totals.blobs.saturating_add(1);
            totals.chunk_refs = totals.chunk_refs.saturating_add(blob.chunk_refs);
            totals.declared_blob_bytes = totals.declared_blob_bytes.saturating_add(blob.size);
            missing_blob_metadata = missing_blob_metadata.saturating_add(blob_metadata_bytes(blob));
        }
    }
    totals.canonical_metadata_bytes = totals
        .canonical_metadata_bytes
        .saturating_sub(all_blob_metadata)
        .saturating_add(missing_blob_metadata);
    totals
}

use oak_core::protocol::{
    STAGED_WINDOW_BUDGET_PROTOCOL, STAGED_WINDOW_MAX_BLOBS,
    STAGED_WINDOW_MAX_RESOLVED_MANIFEST_ENTRIES,
};

fn validate_push_operation_caps(totals: PushOperationTotals) -> Result<()> {
    validate_push_operation_caps_for_budget(totals, false)
}

fn validate_push_operation_caps_for_budget(
    totals: PushOperationTotals,
    window_budget: bool,
) -> Result<()> {
    let checks = [
        (
            "commits",
            totals.commits,
            oak_core::protocol::STAGED_OPERATION_MAX_COMMITS,
        ),
        (
            "tree objects",
            totals.trees,
            oak_core::protocol::STAGED_MAX_TREE_OBJECTS,
        ),
        (
            "direct tree entries",
            totals.direct_tree_entries,
            oak_core::protocol::STAGED_MAX_DIRECT_TREE_ENTRIES,
        ),
        (
            "resolved manifest entries",
            if window_budget {
                totals.window_resolved_entries
            } else {
                totals.resolved_manifest_entries
            },
            if window_budget {
                STAGED_WINDOW_MAX_RESOLVED_MANIFEST_ENTRIES
            } else {
                oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES
            },
        ),
        (
            "file changes",
            totals.file_changes,
            oak_core::protocol::STAGED_MAX_FILE_CHANGES,
        ),
        (
            "canonical metadata bytes",
            totals.canonical_metadata_bytes,
            oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES,
        ),
        (
            "expanded path bytes",
            totals.expanded_path_bytes,
            oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES,
        ),
        (
            "chunk references",
            totals.chunk_refs,
            oak_core::protocol::STAGED_MAX_CHUNK_REFS,
        ),
        (
            "blobs",
            totals.blobs,
            if window_budget {
                STAGED_WINDOW_MAX_BLOBS
            } else {
                oak_core::protocol::STAGED_MAX_BLOBS
            },
        ),
    ];
    for (dimension, actual, limit) in checks {
        if actual > limit {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected the whole operation: {dimension} total {actual} exceeds limit {limit}; use a server advertising staged_budget_protocol=window_v1 for larger staged operations, or reduce the outgoing history/content to fit the supported cumulative limits. No remote state was mutated"
            )));
        }
    }
    if totals.declared_blob_bytes > oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected the whole operation: declared blob bytes total {} exceeds limit {}; reduce the outgoing content to fit this cumulative limit. No remote state was mutated",
            totals.declared_blob_bytes,
            oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES
        )));
    }
    Ok(())
}

fn staged_protocol_required(totals: PushOperationTotals) -> bool {
    totals.commits > oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS
        || totals.blobs > oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
        || totals.chunk_refs > oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS
        || totals.declared_blob_bytes > STAGED_CLIENT_BLOB_BATCH_BYTES
}

#[cfg(test)]
fn select_staged_protocol(totals: PushOperationTotals) -> Result<bool> {
    let staged = staged_protocol_required(totals);
    if staged {
        validate_push_operation_caps(totals)?;
    }
    Ok(staged)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushTransport {
    LegacyOrdinary,
    /// The server's staged workers are not ready, but it advertises
    /// `headless_preload_v1` and only the remote-missing blob totals exceed
    /// one ordinary request: preload those blobs in bounded headless
    /// requests, then publish through the unchanged ordinary path.
    OrdinaryBlobPreload,
    PhaseOneOrdinaryBootstrap,
    StagedReady,
    StagedWindowReady,
    StagedRequiredUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerPushCapability {
    Legacy,
    PhaseOneOrdinaryBootstrap,
    StagedReady,
}

#[derive(Debug, Clone, Copy)]
struct NegotiatedServerPushCapability {
    transport: ServerPushCapability,
    window_budget: bool,
    content_receipt_enforcement_required: bool,
}

#[derive(Debug, Clone, Copy)]
struct FirstPublicationState {
    repo_needs_creation: bool,
    phase_one_ordinary_allowed: bool,
    /// Why this operation may never be a blob preload followed by one
    /// publication, whatever the server offers. Part of the decision so
    /// push and `push --plan` cannot disagree.
    blob_preload_excluded: Option<&'static str>,
}

/// `main` is bootstrapped only into a repository without one, as one
/// self-contained first publication: never as a preload-then-publish. Push
/// and `push --plan` both feed this into the shared transport decision.
const MAIN_BOOTSTRAP_PRELOAD_EXCLUSION: &str =
    "the first publication of `main` is not self-contained";

/// The selected transport plus what it was selected from, so the caller can
/// execute a blob preload or report exactly why publication is impossible.
struct PushTransportDecision {
    transport: PushTransport,
    /// Operation totals counting only blobs the server reported missing.
    totals: PushOperationTotals,
    /// `None` when the push fits one ordinary request and no probe was sent.
    capability: Option<ServerPushCapability>,
    /// Bounded headless requests for [`PushTransport::OrdinaryBlobPreload`].
    preload_batches: Vec<Vec<PlannedBlob>>,
    /// Why a blob preload was not selected although the server offers it.
    preload_refusal: Option<String>,
}

impl PushTransportDecision {
    fn ordinary(totals: PushOperationTotals) -> Self {
        Self {
            transport: PushTransport::LegacyOrdinary,
            totals,
            capability: None,
            preload_batches: Vec::new(),
            preload_refusal: None,
        }
    }

    /// Stable label for `oak push --plan --json`.
    fn label(&self) -> &'static str {
        match self.transport {
            PushTransport::LegacyOrdinary => "ordinary",
            PushTransport::OrdinaryBlobPreload => "ordinary_blob_preload",
            PushTransport::PhaseOneOrdinaryBootstrap => "phase_one_bootstrap",
            PushTransport::StagedReady => "staged_v1",
            PushTransport::StagedWindowReady => "staged_window_v1",
            PushTransport::StagedRequiredUnavailable => "unavailable",
        }
    }

    fn unavailable_error(&self) -> OakError {
        staged_required_unavailable_error(
            self.totals,
            self.capability,
            self.preload_refusal.as_deref(),
        )
    }
}

/// The totals that remain once every remote-missing blob has been preloaded:
/// what the final ordinary publication request itself must fit.
fn totals_without_blobs(mut totals: PushOperationTotals) -> PushOperationTotals {
    totals.blobs = 0;
    totals.chunk_refs = 0;
    totals.declared_blob_bytes = 0;
    totals
}

async fn select_push_transport(
    client: &reqwest::Client,
    remote: &str,
    api_key: Option<&str>,
    totals: PushOperationTotals,
    allow_phase_one_ordinary_bootstrap: bool,
    allow_ordinary_blob_preload: bool,
) -> Result<(PushTransport, Option<ServerPushCapability>)> {
    if !staged_protocol_required(totals) {
        return Ok((PushTransport::LegacyOrdinary, None));
    }
    // Refuse even the expanded cumulative budget before any staging. Only
    // an explicit complete capability set may lift the legacy operation caps.
    validate_push_operation_caps_for_budget(totals, true)?;
    let capability = server_push_capability(client, remote, api_key).await?;
    output::vlog(&format!(
        "push: admitted operation budget candidate {totals:?}; window_budget={}",
        capability.window_budget
    ));
    if !capability.window_budget {
        validate_push_operation_caps(totals)?;
    }
    let transport = match capability.transport {
        ServerPushCapability::StagedReady if capability.window_budget => {
            PushTransport::StagedWindowReady
        }
        ServerPushCapability::StagedReady => PushTransport::StagedReady,
        ServerPushCapability::PhaseOneOrdinaryBootstrap if allow_phase_one_ordinary_bootstrap => {
            PushTransport::PhaseOneOrdinaryBootstrap
        }
        // Blobs are immutable, content-addressed and referenced by no head
        // until a commit names them, so uploading them ahead of the single
        // ordinary publication changes nothing a reader can observe. Commit
        // count is different: it bounds the publication request itself.
        ServerPushCapability::PhaseOneOrdinaryBootstrap
            if allow_ordinary_blob_preload
                && !staged_protocol_required(totals_without_blobs(totals)) =>
        {
            PushTransport::OrdinaryBlobPreload
        }
        ServerPushCapability::Legacy | ServerPushCapability::PhaseOneOrdinaryBootstrap => {
            PushTransport::StagedRequiredUnavailable
        }
    };
    Ok((transport, Some(capability.transport)))
}

async fn select_push_transport_for_plan(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    totals: PushOperationTotals,
    objects: &PlannedObjects,
    first_publication: FirstPublicationState,
) -> Result<PushTransportDecision> {
    if !staged_protocol_required(totals) {
        return Ok(PushTransportDecision::ordinary(totals));
    }
    let missing = if first_publication.repo_needs_creation {
        objects.blobs.iter().map(|blob| blob.hash.clone()).collect()
    } else {
        let unproven: Vec<PlannedBlob> = objects
            .blobs
            .iter()
            .filter(|blob| !objects.server_has_blobs.contains(&blob.hash))
            .cloned()
            .collect();
        remote_missing_staged_blobs(client, remote, endpoint_path, api_key, &unproven, false)
            .await?
    };
    select_push_transport_for_missing(
        client,
        remote,
        api_key,
        totals,
        objects,
        &missing,
        first_publication,
    )
    .await
}

/// [`select_push_transport_for_plan`] for a caller that already asked the
/// server which outgoing blobs it is missing.
async fn select_push_transport_for_missing(
    client: &reqwest::Client,
    remote: &str,
    api_key: Option<&str>,
    totals: PushOperationTotals,
    objects: &PlannedObjects,
    missing: &std::collections::HashSet<Hash>,
    first_publication: FirstPublicationState,
) -> Result<PushTransportDecision> {
    let totals = totals_for_remote_missing_blobs(totals, objects, missing);
    // A preload sends blobs to a repository that already exists; creating
    // one stays behind the first-publication paths.
    let (mut transport, capability) = select_push_transport(
        client,
        remote,
        api_key,
        totals,
        first_publication.phase_one_ordinary_allowed,
        !first_publication.repo_needs_creation && first_publication.blob_preload_excluded.is_none(),
    )
    .await?;
    let mut preload_batches = Vec::new();
    let mut preload_refusal = first_publication
        .blob_preload_excluded
        .filter(|_| {
            transport == PushTransport::StagedRequiredUnavailable
                && capability == Some(ServerPushCapability::PhaseOneOrdinaryBootstrap)
                && !staged_protocol_required(totals_without_blobs(totals))
        })
        .map(str::to_string);
    if transport == PushTransport::OrdinaryBlobPreload {
        let blobs: Vec<PlannedBlob> = objects
            .blobs
            .iter()
            .filter(|blob| missing.contains(&blob.hash))
            .cloned()
            .collect();
        match split_ordinary_preload_batches(blobs) {
            Ok(batches) => preload_batches = batches,
            Err(reason) => {
                transport = PushTransport::StagedRequiredUnavailable;
                preload_refusal = Some(reason);
            }
        }
    }
    Ok(PushTransportDecision {
        transport,
        totals,
        capability,
        preload_batches,
        preload_refusal,
    })
}

/// Split remote-missing blobs into headless preload requests that each stay
/// inside the ordinary publication envelope: at most 128 blobs, 64 MiB of
/// declared content and 10,000 chunk references. A single blob larger than
/// 64 MiB travels alone; its bytes go through the chunk endpoints and the
/// request carries only its chunk references, exactly as an ordinary push of
/// one large file does.
fn split_ordinary_preload_batches(
    blobs: Vec<PlannedBlob>,
) -> std::result::Result<Vec<Vec<PlannedBlob>>, String> {
    let max_blobs = oak_core::protocol::MAPPING_PROOF_MAX_BLOBS;
    let max_chunk_refs = oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS;
    let mut batches = Vec::new();
    let mut current: Vec<PlannedBlob> = Vec::new();
    let mut current_bytes = 0u64;
    let mut current_chunk_refs = 0usize;
    for blob in blobs {
        if blob.chunk_refs > max_chunk_refs {
            return Err(format!(
                "blob {} alone carries {} chunk references > {max_chunk_refs}",
                blob.hash, blob.chunk_refs
            ));
        }
        if !current.is_empty()
            && (current.len() == max_blobs
                || current_bytes.saturating_add(blob.size) > STAGED_CLIENT_BLOB_BATCH_BYTES
                || current_chunk_refs.saturating_add(blob.chunk_refs) > max_chunk_refs)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
            current_chunk_refs = 0;
        }
        current_bytes = current_bytes.saturating_add(blob.size);
        current_chunk_refs = current_chunk_refs.saturating_add(blob.chunk_refs);
        current.push(blob);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

/// Each measured total that exceeds the ordinary publication envelope, as
/// `"<measured> > <limit>"`.
fn ordinary_envelope_excess(totals: PushOperationTotals) -> Vec<String> {
    let mut excess = Vec::new();
    if totals.commits > oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS {
        excess.push(format!(
            "{} outgoing commits > {}",
            totals.commits,
            oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS
        ));
    }
    if totals.blobs > oak_core::protocol::MAPPING_PROOF_MAX_BLOBS {
        excess.push(format!(
            "{} new blobs > {}",
            totals.blobs,
            oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
        ));
    }
    if totals.declared_blob_bytes > STAGED_CLIENT_BLOB_BATCH_BYTES {
        excess.push(format!(
            "{} new blob bytes > {} (64 MiB)",
            totals.declared_blob_bytes, STAGED_CLIENT_BLOB_BATCH_BYTES
        ));
    }
    if totals.chunk_refs > oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS {
        excess.push(format!(
            "{} new chunk references > {}",
            totals.chunk_refs,
            oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS
        ));
    }
    excess
}

/// The deterministic refusal for a push that needs `staged_v1` on a server
/// that has not activated it and offers no bridge for this operation. It is a
/// server-capability outcome (exit 6, `push_capability_unavailable`), never a
/// usage error, and it is returned before the first mutating request.
fn staged_required_unavailable_error(
    totals: PushOperationTotals,
    capability: Option<ServerPushCapability>,
    preload_refusal: Option<&str>,
) -> OakError {
    let excess = ordinary_envelope_excess(totals);
    let measured = if excess.is_empty() {
        String::new()
    } else {
        format!(" ({})", excess.join("; "))
    };
    let blob_only = !staged_protocol_required(totals_without_blobs(totals));
    let bridge = match (capability, preload_refusal) {
        (_, Some(reason)) => format!(
            ", and its headless blob preload (ordinary_bootstrap_protocol=headless_preload_v1) cannot carry this content: {reason}"
        ),
        (Some(ServerPushCapability::PhaseOneOrdinaryBootstrap), None) if blob_only => {
            ", and its headless blob preload (ordinary_bootstrap_protocol=headless_preload_v1) needs a remote repository that already exists".to_string()
        }
        (Some(ServerPushCapability::PhaseOneOrdinaryBootstrap), None) => {
            ", and its headless blob preload (ordinary_bootstrap_protocol=headless_preload_v1) lifts only the new-blob count and byte limits".to_string()
        }
        _ => ", and it does not advertise ordinary_bootstrap_protocol=headless_preload_v1, so new blobs cannot be preloaded ahead of one ordinary publication".to_string(),
    };
    OakError::PushCapabilityUnavailable {
        message: format!(
            "this push exceeds the safe ordinary publication envelope{measured} and requires staged_v1, but the server has not advertised the complete staged capability set{bridge}; the commits stay safe locally and publish unchanged once the server activates staged_v1; no remote state was mutated"
        ),
        recommended_next_commands: vec!["oak push --plan --json".to_string()],
    }
}

fn outgoing_operation_is_self_contained(commits: &[oak_core::Commit]) -> bool {
    let hashes: std::collections::HashSet<&Hash> =
        commits.iter().map(|commit| &commit.hash).collect();
    commits.iter().all(|commit| {
        [
            commit.parent_hash.as_ref(),
            commit.merge_parent_hash.as_ref(),
        ]
        .into_iter()
        .flatten()
        .all(|parent| hashes.contains(parent))
    })
}

const BLOB_CHECK_PAGE_HASHES: usize = 10_000;

fn validate_blob_check_missing(
    requested: &[String],
    returned: Vec<String>,
    context: &str,
) -> Result<std::collections::HashSet<String>> {
    let expected: std::collections::HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut missing = std::collections::HashSet::new();
    for hash in returned {
        if !expected.contains(hash.as_str()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned unrequested hash {hash}; no remote state was mutated"
            )));
        }
        if !missing.insert(hash.clone()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned duplicate hash {hash}; no remote state was mutated"
            )));
        }
    }
    Ok(missing)
}

fn validate_receipt_predicate_ack(
    response: &BlobCheckResponse,
    required: bool,
    context: &str,
) -> Result<()> {
    if required && !response.verified_receipts_required {
        return Err(OakError::InvalidArgument(format!(
            "{context} did not acknowledge the verified-receipt predicate; a server replica may be too old for this publication. Retry after every replica is upgraded. No remote state was mutated"
        )));
    }
    Ok(())
}

async fn confirm_staged_missing_blobs_available(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    missing: &std::collections::HashSet<Hash>,
) -> Result<()> {
    let mut hashes: Vec<String> = missing.iter().map(ToString::to_string).collect();
    hashes.sort();
    for page in hashes.chunks(BLOB_CHECK_PAGE_HASHES) {
        let request = with_auth(
            client
                .post(format!("{remote}/api/{endpoint_path}/blobs/check"))
                .json(&BlobCheckRequest {
                    hashes: page.to_vec(),
                    require_verified_receipts: true,
                    verify_content: false,
                }),
            api_key,
        );
        let response = crate::http::send_idempotent_with_retry_until(
            request,
            "staged missing-blob availability check",
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase could not check locally missing staged blobs: {error}; no remote state was mutated"
            ))
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = crate::http::error_text(response).await;
            return Err(OakError::InvalidArgument(format!(
                "push admission phase could not check locally missing staged blobs: blobs/check returned {status} ({detail}); no remote state was mutated"
            )));
        }
        let proof: BlobCheckResponse = response.json().await.map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase could not decode the staged blob availability response: {error}; no remote state was mutated"
            ))
        })?;
        validate_receipt_predicate_ack(&proof, true, "staged missing-blob availability")?;
        let missing =
            validate_blob_check_missing(page, proof.missing, "staged missing-blob availability")?;
        if !missing.is_empty() {
            let mut missing: Vec<String> = missing.into_iter().collect();
            missing.sort();
            return Err(OakError::InvalidArgument(format!(
                "push admission phase found locally missing staged blob(s) {} unavailable on the server; restore them before retrying. No remote state was mutated",
                missing.join(", ")
            )));
        }
    }
    Ok(())
}

async fn remote_missing_staged_blobs(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    blobs: &[PlannedBlob],
    require_verified_receipts: bool,
) -> Result<std::collections::HashSet<Hash>> {
    let mut missing = std::collections::HashSet::new();
    for page in blobs.chunks(BLOB_CHECK_PAGE_HASHES) {
        let hashes: Vec<String> = page.iter().map(|blob| blob.hash.to_string()).collect();
        let request = with_auth(
            client
                .post(format!("{remote}/api/{endpoint_path}/blobs/check"))
                .json(&BlobCheckRequest {
                    hashes: hashes.clone(),
                    require_verified_receipts,
                    verify_content: false,
                }),
            api_key,
        );
        let response = crate::http::send_idempotent_with_retry_until(
            request,
            "staged blob-content check",
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase could not check staged blob content: {error}; no remote state was mutated"
            ))
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = crate::http::error_text(response).await;
            return Err(OakError::InvalidArgument(format!(
                "push admission phase could not check staged blob content: blobs/check returned {status} ({detail}); no remote state was mutated"
            )));
        }
        let proof: BlobCheckResponse = response.json().await.map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase could not decode staged blob content check: {error}; no remote state was mutated"
            ))
        })?;
        validate_receipt_predicate_ack(&proof, require_verified_receipts, "staged blob check")?;
        let checked = validate_blob_check_missing(&hashes, proof.missing, "staged blob check")?;
        for hash in checked {
            missing.insert(Hash::from_hex(&hash).map_err(|error| {
                OakError::InvalidArgument(format!(
                    "staged blob check returned an invalid hash {hash}: {error}; no remote state was mutated"
                ))
            })?);
        }
    }
    Ok(missing)
}

#[derive(Debug, Clone)]
struct PreparedMappingProof {
    descriptor: BlobProofDescriptor,
    chunks: Vec<BlobProofChunk>,
}

/// Reads a blob's bytes back through its locally stored chunk mapping,
/// verifying each chunk as it is read.
struct StoredChunkReader<'a> {
    repo: &'a SqliteRepository,
    chunks: std::vec::IntoIter<oak_core::ChunkInfo>,
    current: std::io::Cursor<Vec<u8>>,
}

impl Read for StoredChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let read = self.current.read(buf)?;
            if read > 0 || buf.is_empty() {
                return Ok(read);
            }
            let Some(chunk) = self.chunks.next() else {
                return Ok(0);
            };
            let bytes = self
                .repo
                .get_chunk(&chunk.hash)
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("local chunk {} is missing", chunk.hash),
                    )
                })?;
            if bytes.len() != chunk.length as usize || hash_bytes(&bytes) != chunk.hash {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("local chunk {} fails size/hash verification", chunk.hash),
                ));
            }
            self.current = std::io::Cursor::new(bytes);
        }
    }
}

/// A sequential, bounded-memory reader over a locally held blob, whichever
/// way the local store holds it: as chunks under a stored mapping (how a
/// pulled blob is kept) or as one content row (how a committed blob is
/// kept). At most one stored chunk, or about a megabyte of a row, is
/// resident.
enum LocalBlobReader<'a> {
    Chunks(StoredChunkReader<'a>),
    Row(oak_core::StoredBlobReader),
}

impl Read for LocalBlobReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Chunks(reader) => reader.read(buf),
            Self::Row(reader) => reader.read(buf),
        }
    }
}

fn open_local_blob<'a>(
    repo: &'a SqliteRepository,
    hash: &Hash,
) -> Result<Option<LocalBlobReader<'a>>> {
    if let Some(mut mapping) = repo.get_blob_chunks(hash)? {
        if !mapping.is_empty() {
            mapping.sort_by_key(|chunk| chunk.offset);
            return Ok(Some(LocalBlobReader::Chunks(StoredChunkReader {
                repo,
                chunks: mapping.into_iter(),
                current: std::io::Cursor::new(Vec::new()),
            })));
        }
    }
    Ok(repo
        .stored_blob_reader(hash)?
        .map(|(_, reader)| LocalBlobReader::Row(reader)))
}

/// Stream a locally held blob and require it to be exactly `size` bytes
/// hashing to `hash`, without holding it in memory.
fn verify_local_blob_stream(repo: &SqliteRepository, hash: &Hash, size: u64) -> Result<()> {
    let mut reader = open_local_blob(repo, hash)?.ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {hash}: metadata exists but bytes are missing; no remote state was mutated"
        ))
    })?;
    let mut hasher = blake3::Hasher::new();
    let read = std::io::copy(&mut reader, &mut hasher).map_err(|error| {
        OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {hash}: {error}; no remote state was mutated"
        ))
    })?;
    if read != size {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {hash}: stored size {size} does not match decoded content size {read}; no remote state was mutated"
        )));
    }
    let actual = Hash(hasher.finalize().to_hex().to_string());
    if &actual != hash {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {hash}: content hash mismatch (decoded bytes hash to {actual}); no remote state was mutated"
        )));
    }
    Ok(())
}

/// The one chunk mapping (`crate::transfer`) of a locally held blob, cut
/// lazily from the local store: one chunk's bytes at a time. However the
/// blob is stored — a content row, or chunks cut by some earlier transfer —
/// the bytes are streamed through the mapping function, so the mapping on
/// the wire depends on the bytes alone and nothing is written locally. The
/// stream ends with an error unless the bytes were exactly `size` long and
/// hash to `hash`.
struct LocalBlobChunks<'a> {
    mapping: Box<dyn Iterator<Item = Result<(ChunkRef, Vec<u8>)>> + Send + 'a>,
    hasher: blake3::Hasher,
    hash: Hash,
    finished: bool,
}

impl<'a> LocalBlobChunks<'a> {
    fn open(repo: &'a SqliteRepository, hash: &Hash, size: u64) -> Result<Self> {
        let reader = open_local_blob(repo, hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "push admission phase lost staged blob {hash}; no branch state was mutated"
            ))
        })?;
        Ok(Self {
            mapping: Box::new(transfer::map_blob_reader(size, reader)),
            hasher: blake3::Hasher::new(),
            hash: hash.clone(),
            finished: false,
        })
    }
}

impl Iterator for LocalBlobChunks<'_> {
    type Item = Result<(ChunkRef, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let unreadable = |hash: &Hash, detail: String| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected local blob {hash}: {detail}; no branch state was mutated"
            ))
        };
        match self.mapping.next() {
            Some(Ok((chunk, bytes))) => {
                self.hasher.update(&bytes);
                Some(Ok((chunk, bytes)))
            }
            Some(Err(error)) => {
                self.finished = true;
                Some(Err(unreadable(&self.hash, error.to_string())))
            }
            None => {
                self.finished = true;
                let actual = Hash(self.hasher.finalize().to_hex().to_string());
                (actual != self.hash).then(|| {
                    Err(unreadable(
                        &self.hash,
                        format!("content hash mismatch (decoded bytes hash to {actual})"),
                    ))
                })
            }
        }
    }
}

/// The chunk mapping of a locally held content-defined blob, as references
/// only (one streaming pass, nothing retained but the references).
fn local_blob_mapping(repo: &SqliteRepository, hash: &Hash, size: u64) -> Result<Vec<ChunkRef>> {
    LocalBlobChunks::open(repo, hash, size)?
        .map(|chunk| chunk.map(|(chunk_ref, _)| chunk_ref))
        .collect()
}

/// Whether `blob` is a content-defined blob still to be mapped: it carries
/// neither inline bytes nor chunk references, and its bytes are streamed
/// from the local store when its chunks are needed.
fn is_deferred(blob: &BlobData) -> bool {
    blob.chunks.is_empty() && transfer::is_content_defined(blob.size)
}

/// Give every outgoing blob its wire shape under the one chunk mapping
/// (`crate::transfer`). Every transport's blobs pass through here, so the
/// shape depends on the bytes alone:
///
/// - a content-defined blob (at or above the large-file threshold) always
///   travels as its FastCDC chunks — never inline, never as one chunk. Its
///   bytes are not loaded here: a [`PreparedChunk::Blob`] source cuts them
///   from the local store while they upload;
/// - a smaller blob is one chunk that is the blob itself. It stays inline in
///   the request unless the inline total would approach the request-body
///   envelope, in which case that same single chunk goes through the chunk
///   endpoints instead.
fn map_outgoing_blobs(blobs: &mut [BlobData], sources: &mut Vec<PreparedChunk>) -> Result<()> {
    // Vec<u8> serializes as a JSON number array, so 15 MiB raw is already
    // close to the hosted request-body envelope.
    const MAX_INLINE_BYTES: u64 = 15 * 1024 * 1024;
    let total_inline: u64 = blobs
        .iter()
        .filter(|blob| !transfer::is_content_defined(blob.size))
        .map(|blob| blob.content.len() as u64)
        .sum();
    let spill_small_blobs = total_inline > MAX_INLINE_BYTES;
    for blob in blobs {
        if !blob.chunks.is_empty() {
            continue;
        }
        if transfer::is_content_defined(blob.size) {
            // Never inline: whatever loaded these bytes, the local store
            // still holds them and the upload streams them from there.
            blob.content = Vec::new();
            sources.push(PreparedChunk::Blob {
                hash: Hash::from_hex(&blob.hash)?,
                size: blob.size,
            });
            continue;
        }
        if blob.content.is_empty() || !spill_small_blobs {
            continue;
        }
        let data = std::mem::take(&mut blob.content);
        for (chunk_ref, bytes) in transfer::map_blob(&data)? {
            sources.push(PreparedChunk::Buffered(chunk_ref.clone(), bytes));
            blob.chunks.push(chunk_ref);
        }
    }
    Ok(())
}

/// Prove, before any remote mutation, the exact chunk mapping every chunked
/// blob of a staged batch will be published with. A content-defined blob's
/// mapping is computed here in one streaming pass over the local store
/// (verifying its bytes against its hash); the upload later cuts the same
/// bytes with the same function and is checked against it.
fn prepare_staged_mapping_proofs(
    repo: &SqliteRepository,
    blobs: &mut [BlobData],
) -> Result<Vec<PreparedMappingProof>> {
    let mut proofs = Vec::new();
    for blob in blobs.iter_mut() {
        if is_deferred(blob) {
            if blob.size > oak_core::protocol::MAPPING_PROOF_MAX_BLOB_BYTES {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected blob {}: declared size {} exceeds async_v1 per-blob limit {}; no remote state was mutated",
                    blob.hash,
                    blob.size,
                    oak_core::protocol::MAPPING_PROOF_MAX_BLOB_BYTES
                )));
            }
            blob.chunks = local_blob_mapping(repo, &Hash::from_hex(&blob.hash)?, blob.size)
                .map_err(|error| match error {
                    OakError::InvalidArgument(detail) => OakError::InvalidArgument(
                        detail
                            .replace("no branch state was mutated", "no remote state was mutated"),
                    ),
                    other => other,
                })?;
        }
        if blob.chunks.is_empty() {
            continue;
        }
        // The one chunk mapping keeps every chunk far below these limits
        // for any blob async_v1 admits; a mapping outside them is refused,
        // never re-cut into a transport-specific shape.
        if blob.chunks.len() > oak_core::protocol::MAPPING_PROOF_MAX_BLOB_CHUNK_REFS
            || blob
                .chunks
                .iter()
                .any(|chunk| chunk.size as u64 > oak_core::protocol::MAPPING_PROOF_MAX_CHUNK_BYTES)
        {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected blob {}: its chunk mapping exceeds async_v1's per-blob limits; no remote state was mutated",
                blob.hash
            )));
        }
        let proof_chunks: Vec<BlobProofChunk> = blob
            .chunks
            .iter()
            .map(|chunk| BlobProofChunk {
                hash: chunk.hash.clone(),
                offset: chunk.offset,
                size: chunk.size,
            })
            .collect();
        let total_chunks = u32::try_from(proof_chunks.len()).map_err(|_| {
            OakError::InvalidArgument("async_v1 mapping reference count overflow".to_string())
        })?;
        proofs.push(PreparedMappingProof {
            descriptor: BlobProofDescriptor {
                hash: blob.hash.clone(),
                size: blob.size,
                mapping_digest: oak_core::protocol::blob_mapping_digest(&proof_chunks),
                total_chunks,
            },
            chunks: proof_chunks,
        });
    }
    Ok(proofs)
}

fn preflight_staged_blob_batches(
    repo: &SqliteRepository,
    blobs: Vec<PlannedBlob>,
) -> Result<Vec<Vec<PlannedBlob>>> {
    let batches = split_staged_blob_batches(blobs)?;
    for batch in &batches {
        let mut objects = materialize_planned_blob_batch(repo, batch.clone())?;
        map_outgoing_blobs(&mut objects.blobs, &mut objects.chunks)?;
        prepare_staged_mapping_proofs(repo, &mut objects.blobs)?;
    }
    Ok(batches)
}

fn validate_mapping_proof_completion(
    expected: &std::collections::HashSet<String>,
    proof: BlobProofResponse,
    token: Option<&str>,
) -> Result<String> {
    if let Some(job) = proof.mapping_proof_job.as_ref() {
        if job.status != "complete" || token.is_some_and(|expected| job.token != expected) {
            return Err(OakError::InvalidArgument(
                "asynchronous blob mapping proof returned a mismatched terminal job; retry the whole push so the exact mapping generation can be re-proven. No head was advanced"
                    .to_string(),
            ));
        }
    }
    let mut returned = std::collections::HashSet::new();
    for hash in proof.verified.iter().chain(&proof.missing) {
        if !expected.contains(hash) || !returned.insert(hash.clone()) {
            return Err(OakError::InvalidArgument(
                "asynchronous blob mapping proof returned an unknown or duplicate blob hash; retry after the server rollout converges. No head was advanced"
                    .to_string(),
            ));
        }
    }
    if returned != *expected {
        return Err(OakError::InvalidArgument(
            "asynchronous blob mapping proof returned an incomplete result; retry after the server rollout converges. No head was advanced"
                .to_string(),
        ));
    }
    if !proof.missing.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "asynchronous blob mapping proof could not verify blob(s) {}; restore their exact chunk content before retrying. No head was advanced",
            proof.missing.join(", ")
        )));
    }
    let proof_token = proof.proof_token.ok_or_else(|| {
        OakError::InvalidArgument(
            "asynchronous blob mapping proof completed without a proof_token; retry after every server replica supports async_v1. No head was advanced"
                .to_string(),
        )
    })?;
    if proof_token.is_empty() || token.is_some_and(|expected| proof_token != expected) {
        return Err(OakError::InvalidArgument(
            "asynchronous blob mapping proof returned a missing or mismatched proof_token; retry the whole push. No head was advanced"
                .to_string(),
        ));
    }
    Ok(proof_token)
}

fn pending_mapping_proof_job(proof: BlobProofResponse) -> Result<MappingProofJob> {
    let job = proof.mapping_proof_job.ok_or_else(|| {
        OakError::InvalidArgument(
            "asynchronous blob mapping proof returned HTTP 202 without an opaque job; retry after the server rollout converges. No head was advanced"
                .to_string(),
        )
    })?;
    if job.token.is_empty() || !matches!(job.status.as_str(), "uploading" | "pending" | "running") {
        return Err(OakError::InvalidArgument(
            "asynchronous blob mapping proof returned an invalid pending job; retry after the server rollout converges. No head was advanced"
                .to_string(),
        ));
    }
    Ok(job)
}

const MAPPING_PROOF_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAPPING_PROOF_MAX_RESTARTS: usize = 3;
const STAGED_PUBLICATION_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);
const STAGED_PUBLICATION_RESPONSE_MAX_BYTES: usize = 64 * 1024;
async fn read_staged_publication_body(
    mut response: reqwest::Response,
    timeout: std::time::Duration,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > STAGED_PUBLICATION_RESPONSE_MAX_BYTES as u64)
    {
        return Err(OakError::Server(format!(
            "staged publication response exceeded {STAGED_PUBLICATION_RESPONSE_MAX_BYTES} bytes"
        )));
    }
    tokio::time::timeout(timeout, async move {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| OakError::Http(error.to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > STAGED_PUBLICATION_RESPONSE_MAX_BYTES {
                return Err(OakError::Server(format!(
                    "staged publication response exceeded {STAGED_PUBLICATION_RESPONSE_MAX_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| OakError::Server("staged publication response body timed out".to_string()))?
}

async fn send_staged_publication_with_cap(
    request: reqwest::RequestBuilder,
    body: Vec<u8>,
    request_timeout: std::time::Duration,
    reconciliation_commands: &[String],
    reconciliation_note: Option<&str>,
) -> Result<PushResponse> {
    // A staged session makes an explicitly repeated request idempotent, but an
    // ambiguous acknowledgement still must not trigger a client replay. Send
    // this mutation envelope once; retain the session identity for explicit
    // reconciliation or a future user-directed retry.
    let response = crate::http::send_publication_request_with_reply_timeout(
        request,
        body,
        "staged publication",
        reconciliation_commands,
        request_timeout,
    )
    .await
    .map_err(|error| annotate_publication_error(error, reconciliation_note))?;
    let status = response.status();
    if status.is_success() {
        let body = read_staged_publication_body(response, request_timeout)
            .await
            .map_err(|_| {
                crate::http::publication_unconfirmed(
                    "staged publication",
                    publication_reason(
                        "the successful response did not yield a complete bounded receipt",
                        reconciliation_note,
                    ),
                    reconciliation_commands.to_vec(),
                )
            })?;
        return serde_json::from_slice(&body).map_err(|_| {
            crate::http::publication_unconfirmed(
                "staged publication",
                publication_reason(
                    "the successful response did not contain a valid bounded receipt",
                    reconciliation_note,
                ),
                reconciliation_commands.to_vec(),
            )
        });
    }
    if matches!(status.as_u16(), 404 | 405) {
        return Err(OakError::InvalidArgument(
            "server replica does not support the required staged-v1 push endpoint; upgrade or retry after deployment converges. No head was advanced"
                .to_string(),
        ));
    }
    if crate::http::publication_status_is_unconfirmed(status) {
        return Err(crate::http::publication_unconfirmed(
            "staged publication",
            publication_reason(
                format!("the server returned HTTP {status}"),
                reconciliation_note,
            ),
            reconciliation_commands.to_vec(),
        ));
    }
    Err(OakError::Server(format!(
        "staged publication was rejected with HTTP {status}; the response body was omitted because remote diagnostics may contain secrets"
    )))
}

fn proof_deadline_error() -> OakError {
    OakError::InvalidArgument(
        "asynchronous blob mapping proof exceeded its size-aware staged-session deadline; retry the push to resume or restart exact proof. No head was advanced"
            .to_string(),
    )
}

async fn send_mapping_proof_request(
    request: reqwest::RequestBuilder,
    deadline: tokio::time::Instant,
) -> Result<reqwest::Response> {
    send_mapping_proof_request_with_cap(request, deadline, MAPPING_PROOF_REQUEST_TIMEOUT).await
}

async fn send_mapping_proof_request_with_cap(
    request: reqwest::RequestBuilder,
    deadline: tokio::time::Instant,
    request_timeout: std::time::Duration,
) -> Result<reqwest::Response> {
    let mut backoff = std::time::Duration::from_millis(100);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(proof_deadline_error());
        }
        let attempt = request.try_clone().ok_or_else(|| {
            OakError::InvalidArgument(
                "asynchronous mapping proof request cannot be replayed safely".to_string(),
            )
        })?;
        match attempt.timeout(request_timeout.min(remaining)).send().await {
            Ok(response)
                if matches!(
                    response.status(),
                    reqwest::StatusCode::REQUEST_TIMEOUT
                        | reqwest::StatusCode::TOO_MANY_REQUESTS
                        | reqwest::StatusCode::BAD_GATEWAY
                        | reqwest::StatusCode::SERVICE_UNAVAILABLE
                        | reqwest::StatusCode::GATEWAY_TIMEOUT
                ) => {}
            Ok(response) => return Ok(response),
            Err(_) => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(proof_deadline_error());
        }
        tokio::time::sleep(backoff.min(remaining)).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
    }
}

async fn decode_mapping_proof_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    context: &str,
) -> Result<T> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(proof_deadline_error());
    }
    tokio::time::timeout(
        MAPPING_PROOF_REQUEST_TIMEOUT.min(remaining),
        response.json(),
    )
    .await
    .map_err(|_| proof_deadline_error())?
    .map_err(|error| {
        OakError::InvalidArgument(format!(
            "could not decode {context}: {error}; no head was advanced"
        ))
    })
}

#[derive(Debug)]
enum MappingProofAttemptError {
    Restart(String),
    Fatal(OakError),
}

impl From<OakError> for MappingProofAttemptError {
    fn from(value: OakError) -> Self {
        Self::Fatal(value)
    }
}

async fn restart_or_status_error(
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    context: &str,
) -> MappingProofAttemptError {
    let status = response.status();
    if status == reqwest::StatusCode::CONFLICT {
        if let Ok(error) =
            decode_mapping_proof_json::<ErrorResponse>(response, deadline, "mapping proof conflict")
                .await
        {
            if error.error == oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT {
                return MappingProofAttemptError::Fatal(OakError::InvalidArgument(format!(
                    "{context} failed permanently with {}: the server's live blob mapping changed while it was being proved; retry from a fresh local mapping plan. No head was advanced",
                    error.error
                )));
            }
        }
        MappingProofAttemptError::Restart(format!("{context} returned {status}"))
    } else if status == reqwest::StatusCode::NOT_FOUND {
        MappingProofAttemptError::Restart(format!("{context} returned {status}"))
    } else {
        MappingProofAttemptError::Fatal(OakError::InvalidArgument(format!(
            "{context} returned {status}; no head was advanced"
        )))
    }
}

async fn poll_mapping_proof_job(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    expected: &std::collections::HashSet<String>,
    mut job: MappingProofJob,
    deadline: tokio::time::Instant,
) -> std::result::Result<String, MappingProofAttemptError> {
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(MappingProofAttemptError::Fatal(proof_deadline_error()));
        }
        tokio::time::sleep(
            std::time::Duration::from_millis(job.retry_after_ms.clamp(1, 5_000)).min(remaining),
        )
        .await;
        let response = with_auth(
            client.get(format!(
                "{remote}/api/{endpoint_path}/blobs/proofs/{}",
                urlencoding::encode(&job.token)
            )),
            api_key,
        );
        let response = send_mapping_proof_request(response, deadline).await?;
        if response.status() == reqwest::StatusCode::ACCEPTED {
            let next: BlobProofResponse = decode_mapping_proof_json(
                response,
                deadline,
                "pending asynchronous blob mapping proof",
            )
            .await?;
            let next_job = pending_mapping_proof_job(next)?;
            if next_job.token != job.token {
                return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                    "asynchronous blob mapping proof changed its opaque job token; retry the whole push. No head was advanced"
                        .to_string(),
                )));
            }
            job = next_job;
            continue;
        }
        if response.status().is_success() {
            let proof: BlobProofResponse = decode_mapping_proof_json(
                response,
                deadline,
                "completed asynchronous blob mapping proof",
            )
            .await?;
            return Ok(validate_mapping_proof_completion(
                expected,
                proof,
                Some(&job.token),
            )?);
        }
        return Err(restart_or_status_error(
            response,
            deadline,
            &format!("asynchronous blob mapping proof {}", job.token),
        )
        .await);
    }
}

struct UploadedMappingProof {
    expected: std::collections::HashSet<String>,
    job: MappingProofJob,
    terminal_token: Option<String>,
}

async fn upload_mapping_set_once(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    mappings: &[PreparedMappingProof],
    deadline: tokio::time::Instant,
) -> std::result::Result<UploadedMappingProof, MappingProofAttemptError> {
    let expected: std::collections::HashSet<String> = mappings
        .iter()
        .map(|mapping| mapping.descriptor.hash.clone())
        .collect();
    if expected.len() != mappings.len() {
        return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
            "async-v1 proof set contains duplicate blob hashes; no head was advanced".to_string(),
        )));
    }
    let create = with_auth(
        client
            .post(format!(
                "{remote}/api/{endpoint_path}/blobs/proofs/async-v1"
            ))
            .json(&BlobProofRequest {
                blobs: mappings
                    .iter()
                    .map(|mapping| mapping.descriptor.clone())
                    .collect(),
            }),
        api_key,
    );
    let response = send_mapping_proof_request(create, deadline).await?;
    if response.status().is_success() && response.status() != reqwest::StatusCode::ACCEPTED {
        let proof =
            decode_mapping_proof_json(response, deadline, "completed mapping proof").await?;
        let token = validate_mapping_proof_completion(&expected, proof, None)?;
        return Ok(UploadedMappingProof {
            expected,
            job: MappingProofJob {
                token: token.clone(),
                status: "complete".to_string(),
                retry_after_ms: 0,
            },
            terminal_token: Some(token),
        });
    }
    if response.status() != reqwest::StatusCode::ACCEPTED {
        return Err(restart_or_status_error(response, deadline, "mapping proof create").await);
    }
    let created: BlobProofResponse =
        decode_mapping_proof_json(response, deadline, "mapping proof create response").await?;
    let job = pending_mapping_proof_job(created)?;
    if job.status != "uploading" {
        return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
            "mapping proof create did not return uploading state; no head was advanced".to_string(),
        )));
    }

    for (blob_index, mapping) in mappings.iter().enumerate() {
        for (page_index, chunks) in mapping
            .chunks
            .chunks(oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS)
            .enumerate()
        {
            let first_chunk_index = page_index
                .checked_mul(oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS)
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| {
                    MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                        "mapping proof page index overflow".to_string(),
                    ))
                })?;
            let body = BlobProofPagesRequest {
                pages: vec![BlobProofMappingPage {
                    blob_index: u32::try_from(blob_index).map_err(|_| {
                        MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                            "mapping proof blob index overflow".to_string(),
                        ))
                    })?,
                    first_chunk_index,
                    chunks: chunks.to_vec(),
                }],
            };
            let encoded = serde_json::to_vec(&body).map_err(OakError::from)?;
            if encoded.len() > oak_core::protocol::MAPPING_PROOF_PAGE_BODY_BYTES {
                return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                    format!(
                        "mapping proof page encoded to {} bytes, above async_v1's {}-byte limit; no remote state was mutated",
                        encoded.len(),
                        oak_core::protocol::MAPPING_PROOF_PAGE_BODY_BYTES
                    ),
                )));
            }
            let put = with_auth(
                client
                    .put(format!(
                        "{remote}/api/{endpoint_path}/blobs/proofs/{}/mappings",
                        urlencoding::encode(&job.token)
                    ))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(encoded),
                api_key,
            );
            let response = send_mapping_proof_request(put, deadline).await?;
            if !response.status().is_success() {
                return Err(
                    restart_or_status_error(response, deadline, "mapping proof page").await,
                );
            }
            let accepted: BlobProofPagesResponse =
                decode_mapping_proof_json(response, deadline, "mapping proof page response")
                    .await?;
            if accepted.accepted_chunks != chunks.len() as u32 {
                return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                    "mapping proof page response accepted a different chunk count; no head was advanced"
                        .to_string(),
                )));
            }
        }
    }

    Ok(UploadedMappingProof {
        expected,
        job,
        terminal_token: None,
    })
}

async fn upload_mapping_set_with_restarts(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    mappings: &[PreparedMappingProof],
    deadline: tokio::time::Instant,
) -> Result<UploadedMappingProof> {
    let mut last_restart = None;
    for _ in 0..MAPPING_PROOF_MAX_RESTARTS {
        match upload_mapping_set_once(client, remote, endpoint_path, api_key, mappings, deadline)
            .await
        {
            Ok(uploaded) => return Ok(uploaded),
            Err(MappingProofAttemptError::Restart(detail)) => last_restart = Some(detail),
            Err(MappingProofAttemptError::Fatal(error)) => return Err(error),
        }
    }
    Err(OakError::InvalidArgument(format!(
        "async_v1 mapping pages could not restart safely after {MAPPING_PROOF_MAX_RESTARTS} attempts ({}); no head was advanced",
        last_restart.unwrap_or_else(|| "restart budget exhausted".to_string())
    )))
}

async fn finalize_mapping_set(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    mut uploaded: UploadedMappingProof,
    deadline: tokio::time::Instant,
) -> std::result::Result<String, MappingProofAttemptError> {
    if let Some(token) = uploaded.terminal_token {
        return Ok(token);
    }
    let finalize = with_auth(
        client
            .post(format!(
                "{remote}/api/{endpoint_path}/blobs/proofs/{}/finalize",
                urlencoding::encode(&uploaded.job.token)
            ))
            .json(&BlobProofFinalizeRequest {}),
        api_key,
    );
    let response = send_mapping_proof_request(finalize, deadline).await?;
    if response.status() == reqwest::StatusCode::ACCEPTED {
        let pending: BlobProofResponse =
            decode_mapping_proof_json(response, deadline, "mapping proof finalize response")
                .await?;
        let next_job = pending_mapping_proof_job(pending)?;
        if next_job.token != uploaded.job.token {
            return Err(MappingProofAttemptError::Fatal(OakError::InvalidArgument(
                "mapping proof finalize changed its opaque token; no head was advanced".to_string(),
            )));
        }
        uploaded.job = next_job;
        poll_mapping_proof_job(
            client,
            remote,
            endpoint_path,
            api_key,
            &uploaded.expected,
            uploaded.job,
            deadline,
        )
        .await
    } else if response.status().is_success() {
        let proof: BlobProofResponse =
            decode_mapping_proof_json(response, deadline, "completed mapping proof").await?;
        Ok(validate_mapping_proof_completion(
            &uploaded.expected,
            proof,
            Some(&uploaded.job.token),
        )?)
    } else {
        Err(restart_or_status_error(response, deadline, "mapping proof finalize").await)
    }
}

async fn prove_mapping_set_once(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    mappings: &[PreparedMappingProof],
    deadline: tokio::time::Instant,
) -> std::result::Result<String, MappingProofAttemptError> {
    let uploaded =
        upload_mapping_set_once(client, remote, endpoint_path, api_key, mappings, deadline).await?;
    finalize_mapping_set(client, remote, endpoint_path, api_key, uploaded, deadline).await
}

fn split_staged_blob_batches(blobs: Vec<PlannedBlob>) -> Result<Vec<Vec<PlannedBlob>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0u64;
    let mut current_metadata = 0usize;
    let mut current_chunk_refs = 0usize;
    for blob in blobs {
        let metadata = blob
            .hash
            .as_str()
            .len()
            .saturating_add(16)
            .saturating_add(blob.chunk_refs.saturating_mul(64 + 16));
        if blob.size > STAGED_MAX_BLOB_BYTES
            || metadata > STAGED_MAX_METADATA_BYTES
            || blob.chunk_refs > oak_core::protocol::STAGED_MAX_CHUNK_REFS
        {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected blob {} because it exceeds one staged-v1 admission envelope; no remote state was mutated",
                blob.hash
            )));
        }
        if !current.is_empty()
            && (current.len() == STAGED_MAX_BLOBS.min(oak_core::protocol::MAPPING_PROOF_MAX_BLOBS)
                || current_bytes.saturating_add(blob.size)
                    > STAGED_CLIENT_BLOB_BATCH_BYTES.min(STAGED_MAX_BLOB_BYTES)
                || current_metadata.saturating_add(metadata) > STAGED_MAX_METADATA_BYTES
                || current_chunk_refs.saturating_add(blob.chunk_refs)
                    > oak_core::protocol::MAPPING_PROOF_MAX_SET_CHUNK_REFS)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
            current_metadata = 0;
            current_chunk_refs = 0;
        }
        current_bytes = current_bytes.saturating_add(blob.size);
        current_metadata = current_metadata.saturating_add(metadata);
        current_chunk_refs = current_chunk_refs.saturating_add(blob.chunk_refs);
        current.push(blob);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

/// The verified bytes of a small (not content-defined) locally held blob,
/// whether the local store keeps it as a content row or as chunks. `None`
/// when the local store does not hold its bytes.
fn load_small_local_blob(
    repo: &SqliteRepository,
    hash: &Hash,
    size: u64,
) -> Result<Option<Vec<u8>>> {
    let content = if repo
        .get_blob_chunks(hash)?
        .is_some_and(|mapping| !mapping.is_empty())
    {
        let mut content = Vec::with_capacity(size as usize);
        let Some(mut reader) = open_local_blob(repo, hash)? else {
            return Ok(None);
        };
        reader.read_to_end(&mut content).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected local blob {hash}: {error}; no remote state was mutated"
            ))
        })?;
        content
    } else {
        match repo.get_blob(hash)? {
            Some(blob) => blob.content,
            None => return Ok(None),
        }
    };
    let blob = Blob {
        hash: hash.clone(),
        content,
        size,
    };
    admit_local_blob(&blob, hash)?;
    Ok(Some(blob.content))
}

/// Admit one locally held blob for sending and queue it in the shape
/// `map_outgoing_blobs` expects: a small blob with its verified bytes, a
/// content-defined blob with none (its bytes are streamed and hashed here,
/// unless the planner already did, and again while its chunks upload).
/// `None` when the local store does not hold the blob.
fn admit_outgoing_blob(
    repo: &SqliteRepository,
    hash: &Hash,
    admitted_by_plan: bool,
) -> Result<Option<BlobData>> {
    let Some(size) = repo.get_blob_size(hash)? else {
        return Ok(None);
    };
    let content = if transfer::is_content_defined(size) {
        if !admitted_by_plan {
            verify_local_blob_stream(repo, hash, size)?;
        }
        Vec::new()
    } else {
        match load_small_local_blob(repo, hash, size)? {
            Some(content) => content,
            None => return Ok(None),
        }
    };
    Ok(Some(BlobData {
        hash: hash.to_string(),
        content,
        size,
        chunks: Vec::new(),
        mapping_proof_token: None,
    }))
}

fn materialize_planned_blob_batch(
    repo: &SqliteRepository,
    planned: Vec<PlannedBlob>,
) -> Result<PreplannedObjects> {
    let mut blobs = Vec::with_capacity(planned.len());
    for descriptor in planned {
        let content = if transfer::is_content_defined(descriptor.size) {
            // Admitted (streamed and hashed) when the operation was planned;
            // its bytes stay in the local store until its chunks upload.
            Vec::new()
        } else {
            load_small_local_blob(repo, &descriptor.hash, descriptor.size)?.ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "push admission phase lost staged blob {}; no remote state was mutated",
                    descriptor.hash
                ))
            })?
        };
        blobs.push(BlobData {
            hash: descriptor.hash.to_string(),
            content,
            size: descriptor.size,
            chunks: Vec::new(),
            mapping_proof_token: None,
        });
    }
    Ok(PreplannedObjects {
        trees: Vec::new(),
        blobs,
        chunks: Vec::new(),
    })
}

fn materialize_next_staged_tree_batch(
    repo: &SqliteRepository,
    pending: &mut std::collections::VecDeque<Hash>,
) -> Result<Option<PreplannedObjects>> {
    let mut trees = Vec::new();
    let mut entries = 0usize;
    let mut metadata = 0usize;
    while let Some(hash) = pending.front() {
        let tree = repo.get_tree(hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "push admission phase lost staged tree {hash}; no remote state was mutated"
            ))
        })?;
        let wire = oak_core::protocol::tree_to_wire(&tree);
        oak_core::protocol::tree_data_to_core(&wire).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected staged tree {hash}: {error}; no remote state was mutated"
            ))
        })?;
        let next_entries = tree.entries.len();
        let next_metadata = oak_core::protocol::staged_tree_metadata_bytes(&wire);
        if next_entries > STAGED_MAX_TREE_ENTRIES || next_metadata > STAGED_MAX_METADATA_BYTES {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected tree {hash} because it exceeds one staged-v1 admission envelope; no remote state was mutated"
            )));
        }
        if !trees.is_empty()
            && (trees.len() == STAGED_MAX_TREES
                || entries.saturating_add(next_entries) > STAGED_MAX_TREE_ENTRIES
                || metadata.saturating_add(next_metadata) > STAGED_MAX_METADATA_BYTES)
        {
            break;
        }
        pending.pop_front();
        entries = entries.saturating_add(next_entries);
        metadata = metadata.saturating_add(next_metadata);
        trees.push(wire);
    }
    Ok((!trees.is_empty()).then_some(PreplannedObjects {
        trees,
        blobs: Vec::new(),
        chunks: Vec::new(),
    }))
}

#[cfg(test)]
fn split_staged_tree_batches_with_limits(
    trees: Vec<TreeData>,
    max_trees: usize,
    max_entries: usize,
    max_metadata: usize,
) -> Result<Vec<PreplannedObjects>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_entries = 0usize;
    let mut current_metadata = 0usize;
    for tree in trees {
        let entries = tree.entries.len();
        let metadata = wire_size(&tree)?;
        if entries > max_entries || metadata > max_metadata {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected tree {} because it exceeds one staged-v1 admission envelope; no remote state was mutated",
                tree.hash
            )));
        }
        if !current.is_empty()
            && (current.len() == max_trees
                || current_entries.saturating_add(entries) > max_entries
                || current_metadata.saturating_add(metadata) > max_metadata)
        {
            batches.push(PreplannedObjects {
                trees: std::mem::take(&mut current),
                blobs: Vec::new(),
                chunks: Vec::new(),
            });
            current_entries = 0;
            current_metadata = 0;
        }
        current_entries = current_entries.saturating_add(entries);
        current_metadata = current_metadata.saturating_add(metadata);
        current.push(tree);
    }
    if !current.is_empty() {
        batches.push(PreplannedObjects {
            trees: current,
            blobs: Vec::new(),
            chunks: Vec::new(),
        });
    }
    Ok(batches)
}

fn split_staged_commit_batches(
    commits: Vec<oak_core::Commit>,
    manifest_entries: &std::collections::HashMap<Hash, usize>,
    manifest_path_bytes: &std::collections::HashMap<Hash, usize>,
) -> Result<Vec<Vec<oak_core::Commit>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_files = 0usize;
    let mut current_entries = 0usize;
    let mut current_metadata = 0usize;
    let mut current_path_bytes = 0usize;
    let mut current_roots = std::collections::HashSet::new();
    for commit in commits {
        let files = commit.files.len();
        let entries = manifest_entries
            .get(&commit.manifest_hash)
            .copied()
            .unwrap_or(0);
        let metadata = oak_core::protocol::staged_commit_metadata_bytes(&commit_to_wire(&commit));
        let root_path_bytes = manifest_path_bytes
            .get(&commit.manifest_hash)
            .copied()
            .unwrap_or(0);
        let new_root_path_bytes = usize::from(!current_roots.contains(&commit.manifest_hash))
            .saturating_mul(root_path_bytes);
        if files > STAGED_MAX_FILE_CHANGES
            || entries > STAGED_MAX_RESOLVED_ENTRIES
            || metadata > STAGED_MAX_METADATA_BYTES
            || new_root_path_bytes > oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES
        {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected commit {} because it exceeds one staged-v1 admission envelope; no remote state was mutated",
                commit.hash
            )));
        }
        if !current.is_empty()
            && (current.len() == BOOTSTRAP_BATCH_SIZE
                || current_files.saturating_add(files) > STAGED_MAX_FILE_CHANGES
                || current_entries.saturating_add(entries) > STAGED_MAX_RESOLVED_ENTRIES
                || current_metadata.saturating_add(metadata) > STAGED_MAX_METADATA_BYTES
                || current_path_bytes.saturating_add(new_root_path_bytes)
                    > oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES)
        {
            batches.push(std::mem::take(&mut current));
            current_files = 0;
            current_entries = 0;
            current_metadata = 0;
            current_path_bytes = 0;
            current_roots.clear();
        }
        current_files = current_files.saturating_add(files);
        current_entries = current_entries.saturating_add(entries);
        current_metadata = current_metadata.saturating_add(metadata);
        if current_roots.insert(commit.manifest_hash.clone()) {
            current_path_bytes = current_path_bytes.saturating_add(root_path_bytes);
        }
        current.push(commit);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

/// Count tree-entry placements, not unique tree objects: the same subtree
/// referenced by two directories is visited twice by server validation.
fn validation_cost_for_root(
    repo: &SqliteRepository,
    root: &Hash,
    costs: &mut std::collections::HashMap<Hash, usize>,
) -> Result<usize> {
    costs.entry(oak_core::Tree::empty_hash()).or_insert(0);
    let mut active = std::collections::HashSet::new();
    let mut stack = vec![(root.clone(), false)];
    while let Some((hash, expanded)) = stack.pop() {
        if costs.contains_key(&hash) {
            continue;
        }
        let tree = repo.get_tree(&hash)?.ok_or_else(|| OakError::InvalidArgument(format!("window push requires parent tree {hash}; fetch its history before retrying. No remote state was mutated")))?;
        if expanded {
            let mut count = tree.entries.len();
            for e in &tree.entries {
                if e.kind == oak_core::TreeEntryKind::Tree {
                    count = count
                        .checked_add(*costs.get(&e.hash).ok_or_else(|| {
                            OakError::InvalidArgument("unresolved child validation budget".into())
                        })?)
                        .ok_or_else(|| {
                            OakError::InvalidArgument("tree validation budget overflow".into())
                        })?;
                }
            }
            if count > STAGED_MAX_RESOLVED_ENTRIES {
                return Err(OakError::InvalidArgument(format!("one tree requires {count} validation placements, exceeding the per-window limit {STAGED_MAX_RESOLVED_ENTRIES}; reduce this snapshot before pushing. No remote state was mutated")));
            }
            active.remove(&hash);
            costs.insert(hash, count);
        } else {
            if !active.insert(hash.clone()) {
                return Err(OakError::InvalidArgument(
                    "cyclic tree validation graph".into(),
                ));
            }
            stack.push((hash, true));
            for e in tree.entries.iter().rev() {
                if e.kind == oak_core::TreeEntryKind::Tree {
                    stack.push((e.hash.clone(), false));
                }
            }
        }
    }
    costs
        .get(root)
        .copied()
        .ok_or_else(|| OakError::InvalidArgument("missing root validation budget".into()))
}

/// Preserve topological commit order while bounding the roots the server
/// actually traverses, including external parent manifests at each boundary.
fn split_window_commit_batches(
    repo: &SqliteRepository,
    commits: Vec<oak_core::Commit>,
    costs: &mut std::collections::HashMap<Hash, usize>,
    path_bytes: &std::collections::HashMap<Hash, usize>,
) -> Result<Vec<Vec<oak_core::Commit>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut roots = std::collections::HashSet::new();
    let mut incoming_roots = std::collections::HashSet::new();
    let (mut work, mut files, mut metadata, mut paths) = (0usize, 0usize, 0usize, 0usize);
    for commit in commits {
        let mut needed = std::collections::HashSet::from([commit.manifest_hash.clone()]);
        for parent in [
            commit.parent_hash.as_ref(),
            commit.merge_parent_hash.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let parent = repo.get_commit(parent)?.ok_or_else(|| OakError::InvalidArgument(format!("window push requires parent commit {parent} for its validation budget; fetch its history before retrying. No remote state was mutated")))?;
            needed.insert(parent.manifest_hash);
        }
        for root in &needed {
            validation_cost_for_root(repo, root, costs)?;
        }
        let commit_metadata =
            oak_core::protocol::staged_commit_metadata_bytes(&commit_to_wire(&commit));
        loop {
            let extra_work = needed
                .difference(&roots)
                .fold(0usize, |n, root| n.saturating_add(costs[root]));
            let extra_paths = if incoming_roots.contains(&commit.manifest_hash) {
                0
            } else {
                path_bytes
                    .get(&commit.manifest_hash)
                    .copied()
                    .ok_or_else(|| {
                        OakError::InvalidArgument("missing manifest path budget".into())
                    })?
            };
            let fits = current.len() < BOOTSTRAP_BATCH_SIZE
                && work.saturating_add(extra_work) <= STAGED_MAX_RESOLVED_ENTRIES
                && files.saturating_add(commit.files.len()) <= STAGED_MAX_FILE_CHANGES
                && metadata.saturating_add(commit_metadata) <= STAGED_MAX_METADATA_BYTES
                && paths.saturating_add(extra_paths)
                    <= oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES;
            if fits {
                work += extra_work;
                files += commit.files.len();
                metadata += commit_metadata;
                paths += extra_paths;
                roots.extend(needed);
                incoming_roots.insert(commit.manifest_hash.clone());
                current.push(commit);
                break;
            }
            if current.is_empty() {
                return Err(OakError::InvalidArgument(format!("commit {} and its parent validation roots exceed one staged window; reduce this snapshot before pushing. No remote state was mutated",commit.hash)));
            }
            batches.push(std::mem::take(&mut current));
            roots.clear();
            incoming_roots.clear();
            work = 0;
            files = 0;
            metadata = 0;
            paths = 0;
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

#[allow(clippy::too_many_arguments)]
async fn publish_staged_plan(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: &str,
    force: bool,
    api_key: Option<&str>,
    repo_needs_creation: bool,
    window_budget: bool,
    expected_head: Option<Hash>,
    target_head: Hash,
    commits: Vec<oak_core::Commit>,
    objects: PlannedObjects,
    progress: Option<&indicatif::ProgressBar>,
    transfer: &Arc<TransferSession>,
) -> Result<()> {
    let client = crate::http::api_client();
    let PlannedObjects {
        tree_hashes,
        blobs,
        manifest_entries,
        mut validation_entries,
        manifest_path_bytes,
        missing_blobs,
        ..
    } = objects;
    if repo_needs_creation && !missing_blobs.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase cannot create a repository with locally missing blob(s) {}; restore them before retrying. No remote state was mutated",
            missing_blobs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !repo_needs_creation {
        confirm_staged_missing_blobs_available(
            &client,
            remote,
            endpoint_path,
            api_key,
            &missing_blobs,
        )
        .await?;
    }
    let remote_missing = if repo_needs_creation {
        blobs.iter().map(|blob| blob.hash.clone()).collect()
    } else {
        // This query chooses the upload delta; it must not physically
        // re-verify every blob reachable from the outgoing commit closure.
        // The staged server fences and proves the exact session membership
        // before its head CAS. Locally missing blobs were separately checked
        // for server availability above because the client cannot upload them.
        remote_missing_staged_blobs(&client, remote, endpoint_path, api_key, &blobs, true).await?
    };
    let remote_missing_blobs: Vec<PlannedBlob> = blobs
        .into_iter()
        .filter(|blob| remote_missing.contains(&blob.hash))
        .collect();
    // Materialize, verify, and (when required) rechunk every remote-missing
    // blob before creating a repository, proof generation, staged session, or
    // uploading any content. The resulting local mappings are durable, so the
    // execution phase rematerializes only content-addressed references.
    let blob_batches = preflight_staged_blob_batches(repo, remote_missing_blobs)?;
    let commit_batches = if window_budget {
        split_window_commit_batches(repo, commits, &mut validation_entries, &manifest_path_bytes)?
    } else {
        split_staged_commit_batches(commits, &manifest_entries, &manifest_path_bytes)?
    };
    let stage_id = uuid::Uuid::new_v4().simple().to_string();
    let abort_expected_head = expected_head.clone();
    let final_handoff = StagedFinalHandoff::default();
    let final_handoff_for_operation = final_handoff.clone();
    let operation = async {
        // Staged batches share one server-side session and stay sequential;
        // each batch's chunks upload concurrently through the one uploader.
        let blob_batch_count = blob_batches.len();
        for (index, batch) in blob_batches.into_iter().enumerate() {
            if index > 0 {
                transfer.report_phase(&format!("Staged {index}/{blob_batch_count} blob batch(es)"));
            }
            let objects = materialize_planned_blob_batch(repo, batch)?;
            Box::pin(push_async_with_endpoint(
                repo,
                work_tree,
                remote,
                endpoint_path,
                Some(branch_name),
                force,
                api_key,
                Some(PreplannedBatch {
                    window_budget,
                    stage_id: Some(stage_id.clone()),
                    expected_head: expected_head.clone(),
                    commits: Vec::new(),
                    staged_protocol: true,
                    finalize: false,
                    target_head: None,
                    objects: Some(objects),
                    final_handoff: None,
                    transfer: transfer.clone(),
                }),
                None,
                false,
            ))
            .await?;
        }
        let mut pending_trees = std::collections::VecDeque::from(tree_hashes);
        while let Some(objects) = materialize_next_staged_tree_batch(repo, &mut pending_trees)? {
            Box::pin(push_async_with_endpoint(
                repo,
                work_tree,
                remote,
                endpoint_path,
                Some(branch_name),
                force,
                api_key,
                Some(PreplannedBatch {
                    window_budget,
                    stage_id: Some(stage_id.clone()),
                    expected_head: expected_head.clone(),
                    commits: Vec::new(),
                    staged_protocol: true,
                    finalize: false,
                    target_head: None,
                    objects: Some(objects),
                    final_handoff: None,
                    transfer: transfer.clone(),
                }),
                None,
                false,
            ))
            .await?;
        }
        for batch in commit_batches {
            let batch_len = batch.len();
            Box::pin(push_async_with_endpoint(
                repo,
                work_tree,
                remote,
                endpoint_path,
                Some(branch_name),
                force,
                api_key,
                Some(PreplannedBatch {
                    window_budget,
                    stage_id: Some(stage_id.clone()),
                    expected_head: expected_head.clone(),
                    commits: batch,
                    staged_protocol: true,
                    finalize: false,
                    target_head: None,
                    objects: Some(PreplannedObjects {
                        trees: Vec::new(),
                        blobs: Vec::new(),
                        chunks: Vec::new(),
                    }),
                    final_handoff: None,
                    transfer: transfer.clone(),
                }),
                None,
                false,
            ))
            .await?;
            if let Some(progress) = progress {
                progress.inc(batch_len as u64);
            }
        }
        let result = Box::pin(push_async_with_endpoint(
            repo,
            work_tree,
            remote,
            endpoint_path,
            Some(branch_name),
            force,
            api_key,
            Some(PreplannedBatch {
                window_budget,
                stage_id: Some(stage_id.clone()),
                expected_head,
                commits: Vec::new(),
                staged_protocol: true,
                finalize: true,
                target_head: Some(target_head),
                objects: Some(PreplannedObjects {
                    trees: Vec::new(),
                    blobs: Vec::new(),
                    chunks: Vec::new(),
                }),
                final_handoff: Some(final_handoff_for_operation.clone()),
                transfer: transfer.clone(),
            }),
            None,
            false,
        ))
        .await;
        if !matches!(&result, Err(OakError::PublicationUnconfirmed { .. })) {
            final_handoff_for_operation.clear();
        }
        result
    };
    let result = await_staged_operation(operation, tokio::signal::ctrl_c(), &final_handoff).await;
    if should_abort_staged_session(&result, &final_handoff) {
        best_effort_abort_staged_session(
            &client,
            remote,
            endpoint_path,
            &stage_id,
            branch_name,
            abort_expected_head.as_ref(),
            api_key,
        )
        .await;
    }
    result
}

fn should_abort_staged_session(result: &Result<()>, final_handoff: &StagedFinalHandoff) -> bool {
    result.is_err()
        && final_handoff.operation_id().is_none()
        && !matches!(result, Err(OakError::PublicationUnconfirmed { .. }))
}

async fn await_staged_operation<Operation, Signal>(
    operation: Operation,
    signal: Signal,
    final_handoff: &StagedFinalHandoff,
) -> Result<()>
where
    Operation: std::future::Future<Output = Result<()>>,
    Signal: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(operation);
    tokio::pin!(signal);
    tokio::select! {
        result = &mut operation => result,
        signal = &mut signal => match final_handoff.operation_id() {
            Some(operation_id) => Err(crate::http::publication_unconfirmed(
                "staged publication",
                signal.map_or_else(
                    |_| format!("cancellation monitoring ended after possible staged-final handoff; durable local operation id {operation_id}"),
                    |_| format!("the process was interrupted after possible staged-final handoff; durable local operation id {operation_id}"),
                ),
                vec!["oak agent state --refresh --json --compact".to_string()],
            )),
            None => Err(OakError::InvalidArgument(signal.map_or_else(
                |error| format!("push cancellation monitor failed before staged finalization: {error}"),
                |_| "push interrupted before staged finalization".to_string(),
            ))),
        },
    }
}

/// Minute-zero bridge for a fixed server whose staged workers are not ready.
/// Immutable blobs and trees are uploaded in bounded, headless ordinary
/// requests; the complete self-contained commit graph is then published by
/// one ordinary request. The capability probe is what distinguishes this
/// behavior from an unknown legacy server.
#[allow(clippy::too_many_arguments)]
async fn publish_phase_one_ordinary_bootstrap(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: &str,
    force: bool,
    api_key: Option<&str>,
    expected_head: Option<Hash>,
    commits: Vec<oak_core::Commit>,
    objects: PlannedObjects,
    progress: Option<&indicatif::ProgressBar>,
    transfer: &Arc<TransferSession>,
) -> Result<()> {
    if expected_head.is_some() || !outgoing_operation_is_self_contained(&commits) {
        return Err(OakError::InvalidArgument(
            "phase-one ordinary bootstrap requires a self-contained first publication; wait for staged_v1 readiness. No remote state was mutated"
                .to_string(),
        ));
    }
    let PlannedObjects {
        tree_hashes,
        blobs,
        missing_blobs,
        ..
    } = objects;
    if !missing_blobs.is_empty() {
        let mut missing: Vec<String> = missing_blobs
            .into_iter()
            .map(|hash| hash.to_string())
            .collect();
        missing.sort();
        return Err(OakError::InvalidArgument(format!(
            "phase-one ordinary bootstrap is missing local blob(s) {}; restore them before retrying. No remote state was mutated",
            missing.join(", ")
        )));
    }

    // Reuse the staged client's bounded materializers, but deliberately send
    // ordinary wire requests. Each preload has no commits and therefore no
    // candidate head; only the final complete commit envelope is visible.
    let blob_batches = preflight_staged_blob_batches(repo, blobs)?;
    let blob_batch_count = blob_batches.len();
    for (index, batch) in blob_batches.into_iter().enumerate() {
        if index > 0 {
            transfer.report_phase(&format!(
                "Preloaded {index}/{blob_batch_count} blob batch(es)"
            ));
        }
        let objects = materialize_planned_blob_batch(repo, batch)?;
        Box::pin(push_async_with_endpoint(
            repo,
            work_tree,
            remote,
            endpoint_path,
            Some(branch_name),
            force,
            api_key,
            Some(PreplannedBatch {
                window_budget: false,
                stage_id: None,
                expected_head: None,
                commits: Vec::new(),
                staged_protocol: false,
                finalize: false,
                target_head: None,
                objects: Some(objects),
                final_handoff: None,
                transfer: transfer.clone(),
            }),
            None,
            false,
        ))
        .await?;
    }
    let mut pending_trees = std::collections::VecDeque::from(tree_hashes);
    while let Some(objects) = materialize_next_staged_tree_batch(repo, &mut pending_trees)? {
        Box::pin(push_async_with_endpoint(
            repo,
            work_tree,
            remote,
            endpoint_path,
            Some(branch_name),
            force,
            api_key,
            Some(PreplannedBatch {
                window_budget: false,
                stage_id: None,
                expected_head: None,
                commits: Vec::new(),
                staged_protocol: false,
                finalize: false,
                target_head: None,
                objects: Some(objects),
                final_handoff: None,
                transfer: transfer.clone(),
            }),
            None,
            false,
        ))
        .await?;
    }
    let commit_count = commits.len();
    Box::pin(push_async_with_endpoint(
        repo,
        work_tree,
        remote,
        endpoint_path,
        Some(branch_name),
        force,
        api_key,
        Some(PreplannedBatch {
            window_budget: false,
            stage_id: None,
            expected_head: None,
            commits,
            staged_protocol: false,
            finalize: false,
            target_head: None,
            objects: Some(PreplannedObjects {
                trees: Vec::new(),
                blobs: Vec::new(),
                chunks: Vec::new(),
            }),
            final_handoff: None,
            transfer: transfer.clone(),
        }),
        None,
        false,
    ))
    .await?;
    if let Some(progress) = progress {
        progress.inc(commit_count as u64);
    }
    Ok(())
}

/// Upload the chunks of `blobs` through the one uploader and record the
/// mapping each content-defined blob was sent with. A
/// [`PreparedChunk::Blob`] source is cut from the local store one
/// negotiation page at a time, so memory does not grow with blob size. When
/// a blob already carries a mapping (a staged proof), the uploaded mapping
/// must be exactly that one.
async fn upload_outgoing_chunks(
    uploader: &ChunkUploader<'_>,
    repo: &SqliteRepository,
    prepared: Vec<PreparedChunk>,
    blobs: &mut [BlobData],
) -> Result<()> {
    type ChunkStream<'a> = Box<dyn Iterator<Item = Result<(String, Vec<u8>)>> + Send + 'a>;
    let mapped: std::sync::Mutex<std::collections::HashMap<String, Vec<ChunkRef>>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
    let mapped_ref = &mapped;
    let chunks = prepared
        .into_iter()
        .flat_map(move |source| -> ChunkStream<'_> {
            match source {
                PreparedChunk::Buffered(chunk_ref, data) => {
                    Box::new(std::iter::once(Ok((chunk_ref.hash, data))))
                }
                PreparedChunk::Blob { hash, size } => {
                    let stream = match LocalBlobChunks::open(repo, &hash, size) {
                        Ok(stream) => stream,
                        Err(error) => return Box::new(std::iter::once(Err(error))),
                    };
                    let key = hash.to_string();
                    mapped_ref
                        .lock()
                        .expect("uploaded mapping lock poisoned")
                        .insert(key.clone(), Vec::new());
                    Box::new(stream.map(move |chunk| {
                        let (chunk_ref, data) = chunk?;
                        let chunk_hash = chunk_ref.hash.clone();
                        mapped_ref
                            .lock()
                            .expect("uploaded mapping lock poisoned")
                            .get_mut(&key)
                            .expect("mapping entry inserted above")
                            .push(chunk_ref);
                        Ok((chunk_hash, data))
                    }))
                }
            }
        });
    uploader.upload(chunks).await?;
    let mut mapped = mapped.into_inner().expect("uploaded mapping lock poisoned");
    for blob in blobs.iter_mut() {
        let Some(uploaded) = mapped.remove(&blob.hash) else {
            continue;
        };
        if blob.chunks.is_empty() {
            blob.chunks = uploaded;
        } else if blob.chunks.len() != uploaded.len()
            || blob.chunks.iter().zip(&uploaded).any(|(proven, sent)| {
                proven.hash != sent.hash || proven.offset != sent.offset || proven.size != sent.size
            })
        {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected blob {}: its local bytes changed between mapping proof and upload; no branch state was mutated",
                blob.hash
            )));
        }
    }
    if let Some(blob) = blobs.iter().find(|blob| is_deferred(blob)) {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase did not map blob {}; no branch state was mutated",
            blob.hash
        )));
    }
    Ok(())
}

/// A deterministic server refusal of one preload request; the caller adds
/// what was and was not changed.
fn preload_refused(detail: String) -> OakError {
    OakError::PushCapabilityUnavailable {
        message: detail,
        recommended_next_commands: Vec::new(),
    }
}

/// Headless preload requests in flight at once. Every request is a
/// blob-only ordinary `/push`: content-addressed, idempotent and coupled to
/// the others only through the repository-head guard, which none of them
/// moves, so they are safe to run concurrently. The hosted server publishes
/// each blob under one per-organization lock, which bounds what more
/// requests can gain; `OAK_PRELOAD_CONCURRENCY` overrides for measurement.
const PRELOAD_CONCURRENCY: usize = 3;

fn preload_concurrency() -> usize {
    std::env::var("OAK_PRELOAD_CONCURRENCY")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(PRELOAD_CONCURRENCY)
}

/// Upload remote-missing blobs to an existing repository ahead of one
/// ordinary publication (`ordinary_bootstrap_protocol: headless_preload_v1`).
///
/// Every request is an ordinary `/push` with no commits, no trees and no
/// branch metadata, so the server stores verified content-addressed blobs
/// and nothing else: no branch row, description, head, publication event or
/// webhook. The requests are idempotent, and a retried push asks the server
/// which blobs it already holds before planning, so an interrupted preload
/// resumes with only the blobs still missing. After the first request has
/// been accepted, up to [`PRELOAD_CONCURRENCY`] requests run at once; after
/// a failure no new request starts, and the ones in flight are allowed to
/// finish so the reported stored count is what the server acknowledged.
#[allow(clippy::too_many_arguments)]
async fn preload_blobs_headless(
    client: &reqwest::Client,
    repo: &SqliteRepository,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    repo_head: Option<Hash>,
    batches: Vec<Vec<PlannedBlob>>,
    transfer: &Arc<TransferSession>,
) -> Result<()> {
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let total_requests = batches.len();
    let total_blobs: usize = batches.iter().map(Vec::len).sum();
    let concurrency = preload_concurrency();
    push_info(&format!(
        "Preloading {total_blobs} new blob(s) in {total_requests} request(s) before publication..."
    ));
    output::vlog(&format!(
        "push: headless blob preload: {total_requests} request(s), up to {concurrency} at once"
    ));
    let request = PreloadRequestContext {
        client,
        repo,
        remote,
        endpoint_path,
        api_key,
        request_path: format!("{remote}/api/{endpoint_path}/push"),
        // Shared so a head observed by one request guards the others too.
        repo_head: std::sync::Mutex::new(repo_head),
        total_requests,
        transfer,
    };
    // The first request runs alone as the admission probe (below), so send
    // the cheapest batch first: the fewest blobs, then the fewest bytes.
    let mut batches = batches;
    let probe = batches
        .iter()
        .enumerate()
        .min_by_key(|(_, batch)| {
            (
                batch.len(),
                batch
                    .iter()
                    .fold(0u64, |total, blob| total.saturating_add(blob.size)),
            )
        })
        .map(|(index, _)| index);
    if let Some(probe) = probe {
        let batch = batches.remove(probe);
        batches.insert(0, batch);
    }
    let mut queue = batches.into_iter().enumerate();
    let mut in_flight = FuturesUnordered::new();
    let mut stored_blobs = 0usize;
    let mut finished_requests = 0usize;
    let mut failure: Option<(usize, OakError)> = None;
    loop {
        // The first request runs alone: it is the admission probe. A server
        // that refuses the preload, a stale head guard or a transient
        // conflict is discovered (and retried) by one request, exactly as
        // when the requests were sequential; only once the server has
        // accepted one do the rest fan out.
        let limit = if finished_requests == 0 {
            1
        } else {
            concurrency
        };
        while failure.is_none() && in_flight.len() < limit {
            let Some((index, batch)) = queue.next() else {
                break;
            };
            let ordinal = index + 1;
            let batch_blobs = batch.len();
            let request = &request;
            in_flight.push(async move {
                (
                    ordinal,
                    batch_blobs,
                    preload_one_batch(request, ordinal, batch).await,
                )
            });
        }
        let Some((ordinal, batch_blobs, outcome)) = in_flight.next().await else {
            break;
        };
        match outcome {
            Ok(()) => {
                stored_blobs += batch_blobs;
                finished_requests += 1;
                transfer.report_phase(&format!(
                    "Preloaded {stored_blobs}/{total_blobs} blob(s) ({finished_requests}/{total_requests} request(s))"
                ));
            }
            Err(error) => {
                failure.get_or_insert((ordinal, error));
            }
        }
    }
    let Some((ordinal, error)) = failure else {
        return Ok(());
    };
    match error {
        error @ (OakError::AuthRequired { .. }
        | OakError::RemoteMoved { .. }
        | OakError::RepoLocked) => Err(error),
        OakError::PushCapabilityUnavailable { message, .. } => {
            Err(OakError::PushCapabilityUnavailable {
                message: format!(
                    "the server refused blob preload for this repository (request {ordinal}/{total_requests}: {message}), and this push exceeds the ordinary publication envelope, so it requires staged_v1, which the server has not activated; no branch state was changed; {stored_blobs} of {total_blobs} new blob(s) were uploaded and remain unreferenced content-addressed objects; the commits stay safe locally and publish unchanged once the server accepts the preload or activates staged_v1"
                ),
                recommended_next_commands: vec!["oak status --json".to_string()],
            })
        }
        error => {
            let detail = match error {
                OakError::Server(detail) | OakError::Http(detail) => detail,
                other => other.to_string(),
            };
            Err(OakError::Server(format!(
                "blob preload request {ordinal}/{total_requests} failed: {detail}. No branch head was moved and nothing was published; {stored_blobs} of {total_blobs} new blob(s) are stored on the server as unreferenced content-addressed objects and will not be re-sent. Retry `oak push`"
            )))
        }
    }
}

struct PreloadRequestContext<'a> {
    client: &'a reqwest::Client,
    repo: &'a SqliteRepository,
    remote: &'a str,
    endpoint_path: &'a str,
    api_key: Option<&'a str>,
    request_path: String,
    repo_head: std::sync::Mutex<Option<Hash>>,
    total_requests: usize,
    transfer: &'a Arc<TransferSession>,
}

/// One headless preload request: upload the batch's chunks through the one
/// uploader, then store its blobs with a blob-only ordinary `/push`.
async fn preload_one_batch(
    context: &PreloadRequestContext<'_>,
    ordinal: usize,
    batch: Vec<PlannedBlob>,
) -> Result<()> {
    const PRELOAD_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    const PRELOAD_HEAD_RETRIES: usize = 3;
    let PreloadRequestContext {
        client,
        repo,
        remote,
        endpoint_path,
        api_key,
        request_path,
        repo_head,
        total_requests,
        transfer,
    } = context;
    let batch_blobs = batch.len();
    let declared_bytes = batch
        .iter()
        .fold(0u64, |total, blob| total.saturating_add(blob.size));
    let mut objects = materialize_planned_blob_batch(repo, batch)?;
    map_outgoing_blobs(&mut objects.blobs, &mut objects.chunks)?;
    let uploader = ChunkUploader {
        remote,
        endpoint_path,
        api_key: *api_key,
        retry: ChunkBusyRetryState::for_upload(declared_bytes),
        session: (*transfer).clone(),
    };
    upload_outgoing_chunks(
        &uploader,
        repo,
        std::mem::take(&mut objects.chunks),
        &mut objects.blobs,
    )
    .await?;
    let mut blobs = objects.blobs;
    let inline_blobs = blobs.iter().filter(|blob| !blob.content.is_empty()).count() as u64;
    let inline_bytes: u64 = blobs.iter().map(|blob| blob.content.len() as u64).sum();
    for attempt in 0..=PRELOAD_HEAD_RETRIES {
        let expected_head = repo_head
            .lock()
            .expect("preload head lock poisoned")
            .as_ref()
            .map(ToString::to_string);
        let request = PushRequest {
            // Without branch metadata the server compares this with
            // the repository head; it is a guard, not a head move.
            expected_head,
            expected_branch_head: None,
            force: false,
            branch: None,
            commits: Vec::new(),
            blobs,
            trees: Vec::new(),
        };
        let body = serde_json::to_vec(&request).map_err(|_| {
            OakError::InvalidArgument("could not encode the blob preload request".to_string())
        })?;
        blobs = request.blobs;
        output::vlog(&format!(
            "push: POST {request_path} (headless blob preload {ordinal}/{total_requests}: 0 commit(s), {batch_blobs} blob(s), 0 tree(s))"
        ));
        let started = std::time::Instant::now();
        let response = crate::http::send_idempotent_with_retry_until(
            with_auth(
                client
                    .post(request_path)
                    .header("x-oak-user", super::commit::get_author())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body),
                *api_key,
            ),
            "blob preload",
            tokio::time::Instant::now() + PRELOAD_REQUEST_TIMEOUT,
        )
        .await?;
        let status = response.status();
        output::vlog(&format!(
            "push: headless blob preload {ordinal}/{total_requests} returned {status} in {:.3}s",
            started.elapsed().as_secs_f64()
        ));
        if status.is_redirection() {
            return Err(crate::http::server_error(response).await);
        }
        if status.as_u16() == 401 {
            return Err(OakError::AuthRequired {
                remote: remote.to_string(),
                status: 401,
            });
        }
        if matches!(status.as_u16(), 409 | 423 | 425) {
            // The server's "changed during push; retry" family:
            // transient, never a statement about capabilities.
            if attempt < PRELOAD_HEAD_RETRIES {
                tokio::time::sleep(std::time::Duration::from_millis(250 * (attempt as u64 + 1)))
                    .await;
                continue;
            }
            return Err(OakError::Server(format!(
                "the server kept answering HTTP {status} (a transient conflict it asks clients to retry)"
            )));
        }
        if status.is_client_error() && !crate::http::retryable_idempotent_status(status) {
            // A deterministic refusal of the preload itself (for
            // example a caller the server lets push only its own
            // branch): not a credential problem, and resending the
            // same request cannot succeed.
            return Err(preload_refused(format!("HTTP {status}")));
        }
        if !status.is_success() {
            return Err(OakError::Server(format!(
                "the server returned HTTP {status}; the response body was omitted because remote diagnostics may contain secrets"
            )));
        }
        let ack: PushResponse = response.json().await.map_err(|_| {
            OakError::Server("the server's reply was not a push receipt".to_string())
        })?;
        if ack.success {
            transfer.record_inline_blobs(inline_blobs, inline_bytes);
            return Ok(());
        }
        // The repository head moved since it was read. Nothing was
        // stored; resend against the head the server reported.
        if attempt == PRELOAD_HEAD_RETRIES {
            break;
        }
        let observed = match ack.new_head {
            Some(head) => Some(Hash::from_hex(&head)?),
            None => None,
        };
        *repo_head.lock().expect("preload head lock poisoned") = observed;
    }
    // `main` genuinely moved under every attempt: transient.
    Err(OakError::Server(
        "the repository head kept moving during preload".to_string(),
    ))
}

/// Trees and blobs the server already holds because they are reachable from
/// a head it reported for this push (the branch head and the repository's
/// `main` head). A server head is published, and a published commit's whole
/// closure is stored, so these need not be re-read, re-verified or re-sent.
///
/// Found by walking each outgoing snapshot alongside the server heads'
/// snapshots path by path: unchanged subtrees are matched by hash and never
/// opened, so the cost follows the changed directories, not the repository.
/// Objects the walk can't prove (moved files, heads not stored locally) are
/// simply not listed and go through the normal blob check.
#[derive(Default)]
struct ServerKnownObjects {
    trees: std::collections::HashSet<Hash>,
    blobs: std::collections::HashSet<Hash>,
}

impl ServerKnownObjects {
    fn has_blob(&self, hash: &Hash) -> bool {
        // The empty blob is always re-sent: a server may hold only a
        // metadata row for it (see the blob-check retain below).
        self.blobs.contains(hash) && *hash != Blob::empty_hash()
    }
}

fn server_known_objects<'a>(
    repo: &SqliteRepository,
    outgoing_roots: impl IntoIterator<Item = &'a Hash>,
    server_heads: &[&Hash],
) -> Result<ServerKnownObjects> {
    let mut known = ServerKnownObjects::default();
    let mut base_roots = Vec::new();
    for head in server_heads {
        if let Some(commit) = repo.get_commit(head)? {
            if commit.manifest_hash != oak_core::Tree::empty_hash() {
                base_roots.push(commit.manifest_hash);
            }
        }
    }
    if base_roots.is_empty() {
        return Ok(known);
    }
    let mut visited = std::collections::HashSet::new();
    let mut stack: Vec<(Hash, Hash)> = Vec::new();
    for root in outgoing_roots {
        for base in &base_roots {
            stack.push((root.clone(), base.clone()));
        }
    }
    while let Some((new, base)) = stack.pop() {
        if new == base {
            known.trees.insert(base);
            continue;
        }
        if !visited.insert((new.clone(), base.clone())) {
            continue;
        }
        let (Some(new_tree), Some(base_tree)) = (repo.get_tree(&new)?, repo.get_tree(&base)?)
        else {
            continue;
        };
        let mut base_subtrees = std::collections::HashMap::new();
        for entry in base_tree.entries {
            match entry.kind {
                oak_core::TreeEntryKind::Blob => {
                    known.blobs.insert(entry.hash);
                }
                oak_core::TreeEntryKind::Tree => {
                    known.trees.insert(entry.hash.clone());
                    base_subtrees.insert(entry.name, entry.hash);
                }
            }
        }
        known.trees.insert(base);
        for entry in new_tree.entries {
            if entry.kind != oak_core::TreeEntryKind::Tree {
                continue;
            }
            if let Some(base_child) = base_subtrees.get(&entry.name) {
                stack.push((entry.hash, base_child.clone()));
            }
        }
    }
    Ok(known)
}

fn collect_planned_objects(
    repo: &SqliteRepository,
    commits: &[&oak_core::Commit],
) -> Result<PlannedObjects> {
    collect_planned_objects_with_known(repo, commits, None)
}

/// [`collect_planned_objects`], counting every object as before but skipping
/// the byte-level re-verification of blobs `known` says the server already
/// holds: they will not be sent, so their local bytes can't affect the push.
fn collect_planned_objects_with_known(
    repo: &SqliteRepository,
    commits: &[&oak_core::Commit],
    known: Option<&ServerKnownObjects>,
) -> Result<PlannedObjects> {
    // Trees whose closure the server holds: the proven ones, plus every
    // subtree reached through one of them.
    let mut known_trees: std::collections::HashSet<Hash> =
        known.map(|known| known.trees.clone()).unwrap_or_default();
    let mut known_pending: Vec<Hash> = Vec::new();
    let mut tree_state: std::collections::HashMap<Hash, u8> = std::collections::HashMap::new();
    let mut seen_blobs = std::collections::HashSet::new();
    let mut missing_blobs = std::collections::HashSet::new();
    let mut tree_hashes = Vec::new();
    let mut blobs = Vec::new();
    let mut manifest_entries = std::collections::HashMap::new();
    let mut validation_entries = std::collections::HashMap::new();
    validation_entries.insert(oak_core::Tree::empty_hash(), 0usize);
    let mut direct_tree_entries = 0usize;
    let mut tree_metadata_bytes = 0usize;
    for commit in commits {
        let wire = commit_to_wire(commit);
        oak_core::protocol::commit_data_to_core(&wire).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing commit {}: {error}; no remote state was mutated",
                commit.hash
            ))
        })?;
    }
    let mut stack: Vec<(Hash, bool)> = commits
        .iter()
        .map(|commit| (commit.manifest_hash.clone(), false))
        .collect();
    manifest_entries.insert(oak_core::Tree::empty_hash(), 0);
    while let Some((hash, expanded)) = stack.pop() {
        if hash == oak_core::Tree::empty_hash() {
            continue;
        }
        if expanded {
            tree_state.insert(hash.clone(), 2);
            let tree = repo
                .get_tree(&hash)
                .map_err(|error| {
                    OakError::InvalidArgument(format!(
                        "push admission phase rejected local tree {hash}: {error}; no remote state was mutated"
                    ))
                })?
                .ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "push admission phase lost local tree {hash} during admission; no remote state was mutated"
                ))
            })?;
            let mut resolved_entries = 0usize;
            let mut validation_cost = tree.entries.len();
            for entry in &tree.entries {
                if entry.kind == oak_core::TreeEntryKind::Tree {
                    validation_cost = validation_cost
                        .checked_add(*validation_entries.get(&entry.hash).ok_or_else(|| {
                            OakError::InvalidArgument(
                                "child tree validation cost unavailable".into(),
                            )
                        })?)
                        .ok_or_else(|| {
                            OakError::InvalidArgument("tree validation cost overflow".into())
                        })?;
                }
                match entry.kind {
                    oak_core::TreeEntryKind::Tree => {
                        resolved_entries = resolved_entries
                            .checked_add(*manifest_entries.get(&entry.hash).ok_or_else(|| {
                                OakError::InvalidArgument(format!(
                                    "push admission phase rejected tree graph: child {} was not admitted before parent {hash}; no remote state was mutated",
                                    entry.hash
                                ))
                            })?)
                            .ok_or_else(|| {
                                OakError::InvalidArgument(
                                    "push admission phase rejected tree graph: resolved entry count overflow; no remote state was mutated".to_string(),
                                )
                            })?;
                    }
                    oak_core::TreeEntryKind::Blob => {
                        resolved_entries = resolved_entries.checked_add(1).ok_or_else(|| {
                            OakError::InvalidArgument(
                                "push admission phase rejected tree graph: resolved entry count overflow; no remote state was mutated".to_string(),
                            )
                        })?;
                        if seen_blobs.insert(entry.hash.clone()) {
                            let server_has = entry.hash != Blob::empty_hash()
                                && (known_trees.contains(&hash)
                                    || known.is_some_and(|known| known.has_blob(&entry.hash)));
                            if server_has {
                                // Sized in one batch after the walk.
                                known_pending.push(entry.hash.clone());
                            } else {
                                match admit_local_blob_descriptor(repo, &entry.hash)? {
                                    Some(blob) => blobs.push(blob),
                                    None => {
                                        missing_blobs.insert(entry.hash.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            manifest_entries.insert(hash.clone(), resolved_entries);
            validation_entries.insert(hash.clone(), validation_cost);
            tree_hashes.push(hash);
            continue;
        }
        match tree_state.get(&hash) {
            Some(1) => {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected outgoing tree graph: cycle at {hash}; no remote state was mutated"
                )));
            }
            Some(2) => continue,
            _ => {}
        }
        let tree = repo
            .get_tree(&hash)
            .map_err(|error| {
                OakError::InvalidArgument(format!(
                    "push admission phase rejected local tree {hash}: {error}; no remote state was mutated"
                ))
            })?
            .ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "push admission phase rejected local tree {hash}: object is missing; no remote state was mutated"
                ))
            })?;
        let wire = oak_core::protocol::tree_to_wire(&tree);
        oak_core::protocol::tree_data_to_core(&wire).map_err(|error| {
            OakError::InvalidArgument(format!(
                "push admission phase rejected outgoing tree {}: {error}; no remote state was mutated",
                tree.hash
            ))
        })?;
        direct_tree_entries = direct_tree_entries.saturating_add(tree.entries.len());
        tree_metadata_bytes = tree_metadata_bytes
            .saturating_add(oak_core::protocol::staged_tree_metadata_bytes(&wire));
        tree_state.insert(hash.clone(), 1);
        let tree_known = known_trees.contains(&hash);
        stack.push((hash, true));
        for entry in tree.entries.into_iter().rev() {
            if entry.kind == oak_core::TreeEntryKind::Tree {
                if tree_known {
                    known_trees.insert(entry.hash.clone());
                }
                stack.push((entry.hash, false));
            }
        }
    }
    let pending_refs: Vec<&Hash> = known_pending.iter().collect();
    let mut server_has_blobs = std::collections::HashSet::new();
    for (hash, descriptor) in known_pending
        .iter()
        .zip(repo.blob_descriptors(&pending_refs)?)
    {
        match descriptor {
            Some((size, chunk_refs)) => {
                server_has_blobs.insert(hash.clone());
                blobs.push(PlannedBlob {
                    hash: hash.clone(),
                    size,
                    chunk_refs,
                });
            }
            None => {
                missing_blobs.insert(hash.clone());
            }
        }
    }
    let manifest_path_bytes =
        expanded_path_bytes_for_roots(repo, commits.iter().map(|commit| &commit.manifest_hash))?;
    let expanded_path_bytes = manifest_path_bytes
        .values()
        .fold(0usize, |total, bytes| total.saturating_add(*bytes));
    Ok(PlannedObjects {
        tree_hashes,
        blobs,
        missing_blobs,
        manifest_entries,
        validation_entries,
        manifest_path_bytes,
        direct_tree_entries,
        tree_metadata_bytes,
        expanded_path_bytes,
        server_has_blobs,
    })
}

fn expanded_path_bytes_for_roots<'a>(
    repo: &SqliteRepository,
    roots: impl IntoIterator<Item = &'a Hash>,
) -> Result<std::collections::HashMap<Hash, usize>> {
    let mut totals = std::collections::HashMap::new();
    totals.insert(oak_core::Tree::empty_hash(), 0);
    let mut operation_total = 0usize;
    let mut seen_roots = std::collections::HashSet::new();
    for root in roots {
        if *root == oak_core::Tree::empty_hash() || !seen_roots.insert(root.clone()) {
            continue;
        }
        let mut root_total = 0usize;
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((hash, prefix_bytes)) = stack.pop() {
            let tree = repo.get_tree(&hash)?.ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "push admission phase cannot size expanded paths because tree {hash} is missing; no remote state was mutated"
                ))
            })?;
            for entry in tree.entries {
                let path_bytes = prefix_bytes
                    .saturating_add(usize::from(prefix_bytes != 0))
                    .saturating_add(entry.name.len());
                root_total = root_total.saturating_add(path_bytes);
                operation_total = operation_total.saturating_add(path_bytes);
                if operation_total > oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES {
                    totals.insert(root.clone(), root_total);
                    return Ok(totals);
                }
                if entry.kind == oak_core::TreeEntryKind::Tree {
                    stack.push((entry.hash, path_bytes));
                }
            }
        }
        totals.insert(root.clone(), root_total);
    }
    Ok(totals)
}

async fn create_remote_repo(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
) -> Result<bool> {
    let Some((owner, repo_name)) = endpoint_path.split_once('/') else {
        return Ok(false);
    };
    push_info(&format!(
        "Creating repository '{owner}/{repo_name}' on server..."
    ));
    let response = with_auth(
        client
            .post(format!("{remote}/api/repos"))
            .json(&serde_json::json!({
                "name": repo_name,
                "description": null,
                "organization_slug": owner,
            })),
        api_key,
    )
    .send()
    .await
    .map_err(|error| OakError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(crate::http::server_error(response).await);
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteRepoPreflight {
    Exists,
    MissingWillCreate,
}

/// Helper to add auth header to a request builder
fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(key) = api_key {
        builder.header("authorization", format!("Bearer {key}"))
    } else {
        builder
    }
}

fn branch_to_push_data(branch: &oak_core::Branch) -> BranchData {
    BranchData {
        name: branch.name.clone(),
        description: branch.description.clone(),
        parent_branch: branch.parent_branch.clone(),
        status: branch.status.to_string(),
        created_at: branch.created_at.to_rfc3339(),
        close_reason: branch
            .close_reason
            .as_ref()
            .map(|reason| reason.as_str().to_string()),
    }
}

#[cfg(test)]
async fn staged_session_capability_available(
    client: &reqwest::Client,
    remote: &str,
    api_key: Option<&str>,
) -> Result<bool> {
    Ok(matches!(
        server_push_capability(client, remote, api_key)
            .await?
            .transport,
        ServerPushCapability::StagedReady
    ))
}

async fn server_push_capability(
    client: &reqwest::Client,
    remote: &str,
    api_key: Option<&str>,
) -> Result<NegotiatedServerPushCapability> {
    let response = with_auth(client.get(format!("{remote}/api/capabilities")), api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    if matches!(response.status().as_u16(), 404 | 405) {
        return Ok(NegotiatedServerPushCapability {
            transport: ServerPushCapability::Legacy,
            window_budget: false,
            content_receipt_enforcement_required: false,
        });
    }
    if !response.status().is_success() {
        return Err(OakError::InvalidArgument(format!(
            "server staged-session capability probe failed (HTTP {}); no remote state was mutated",
            response.status()
        )));
    }
    let capability: serde_json::Value =
        tokio::time::timeout(std::time::Duration::from_secs(10), response.json())
            .await
            .map_err(|_| OakError::Http("server capability response timed out".to_string()))?
            .map_err(|error| OakError::Http(error.to_string()))?;
    let content_receipt_enforcement_required = capability["content_receipt_enforcement_required"]
        .as_bool()
        .unwrap_or(false);
    let staged_ready = capability["push_protocol"] == "staged_v1"
        && capability["staged_capabilities_ready"] == true
        && capability["staged_session_protocol"] == "opaque_v1"
        && capability["mapping_proof_protocol"] == oak_core::protocol::MAPPING_PROOF_PROTOCOL
        && capability["staged_abort_protocol"] == oak_core::protocol::STAGED_ABORT_PROTOCOL;
    if staged_ready {
        return Ok(NegotiatedServerPushCapability {
            transport: ServerPushCapability::StagedReady,
            window_budget: capability["staged_budget_protocol"] == STAGED_WINDOW_BUDGET_PROTOCOL,
            content_receipt_enforcement_required,
        });
    }
    // A fixed server deliberately advertises this exact append-only shape
    // while staged workers/backfills are still coming online. It preserves
    // object-only ordinary pushes (which cannot move a head) and one bounded
    // ordinary publication, unlike an unknown legacy server whose semantics
    // cannot be assumed safely.
    let phase_one_ordinary = capability["push_protocol"] == "legacy"
        && capability["staged_capabilities_ready"] == false
        && capability["staged_session_protocol"] == "opaque_v1"
        && capability["staged_abort_protocol"] == oak_core::protocol::STAGED_ABORT_PROTOCOL
        && capability["known_loss_protocol"] == oak_core::protocol::KNOWN_LOSS_PROTOCOL
        && capability["ordinary_bootstrap_protocol"]
            == oak_core::protocol::ORDINARY_BOOTSTRAP_PROTOCOL;
    Ok(NegotiatedServerPushCapability {
        window_budget: false,
        transport: if phase_one_ordinary {
            ServerPushCapability::PhaseOneOrdinaryBootstrap
        } else {
            ServerPushCapability::Legacy
        },
        content_receipt_enforcement_required,
    })
}

async fn best_effort_abort_staged_session(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    stage_id: &str,
    branch_name: &str,
    expected_head: Option<&Hash>,
    api_key: Option<&str>,
) {
    best_effort_abort_staged_session_with_timeout(
        client,
        remote,
        endpoint_path,
        stage_id,
        branch_name,
        expected_head,
        api_key,
        std::time::Duration::from_secs(3),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn best_effort_abort_staged_session_with_timeout(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    stage_id: &str,
    branch_name: &str,
    expected_head: Option<&Hash>,
    api_key: Option<&str>,
    timeout: std::time::Duration,
) {
    let abort = with_auth(
        client
            .post(format!(
                "{remote}/api/{endpoint_path}/push/staged-v1/{stage_id}/abort"
            ))
            .json(&StagedAbortRequest {
                branch_name: branch_name.to_string(),
                expected_branch_head: expected_head.map(ToString::to_string),
            }),
        api_key,
    );
    let response = match tokio::time::timeout(timeout, abort.send()).await {
        Ok(response) => response,
        Err(_) => {
            output::vlog(&format!(
                "push: staged abort timed out after {}ms; the server will expire the session",
                timeout.as_millis()
            ));
            return;
        }
    };
    match response {
        Ok(response) if response.status().is_success() => {
            output::vlog(&format!("push: released staged session {stage_id}"));
        }
        Ok(response) => output::vlog(&format!(
            "push: staged abort returned {}; the server will expire the session",
            response.status()
        )),
        Err(error) => output::vlog(&format!(
            "push: staged abort could not reach the server ({error}); the server will expire the session"
        )),
    }
}

pub(crate) async fn preflight_remote_repo(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
) -> Result<RemoteRepoPreflight> {
    let remote = remote.trim_end_matches('/');
    let resp = with_auth(client.get(format!("{remote}/api/{endpoint_path}")), api_key)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().is_success() {
        let _repo_info: RepoResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        Ok(RemoteRepoPreflight::Exists)
    } else if resp.status().as_u16() == 404 {
        Ok(RemoteRepoPreflight::MissingWillCreate)
    } else {
        Err(crate::http::server_error(resp).await)
    }
}

/// Piped stdout gets one result line instead of the step-by-step push
/// narration; an interactive terminal keeps the full progress story.
fn quiet_stdout() -> bool {
    use std::io::IsTerminal;
    !std::io::stdout().is_terminal()
}

fn push_info(msg: &str) {
    if !quiet_stdout() {
        output::info(msg);
    }
}

fn push_success(msg: &str) {
    if !quiet_stdout() {
        output::success(msg);
    }
}

#[derive(Debug, serde::Serialize)]
struct PushJson {
    schema_version: u32,
    pushed: bool,
    published: bool,
    remote_contacted: bool,
    remote: String,
    repo: String,
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pushed_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch_pushed_head: Option<String>,
    current_branch_push_checked: bool,
    review_url: String,
    /// Whether the pushed branch still merges into its parent. Absent when
    /// the branch has no merge target.
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_check: Option<MergeCheckJson>,
    /// What this push transferred, from server acknowledgements:
    /// `chunks_required`, `chunks_already_present`, `chunks_uploaded`,
    /// `bytes_uploaded` (see [`TransferCounts`]).
    #[serde(flatten)]
    transfer: TransferCounts,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug)]
struct PushCommandOutcome {
    remote: String,
    repo: String,
    owner: String,
    repo_name: String,
    branch: String,
    published: bool,
    current_branch_pushed_head: Option<String>,
    transfer: TransferCounts,
}

#[derive(Debug)]
struct EndpointPushOutcome {
    published: bool,
    current_branch_pushed_head: Option<Hash>,
    transfer: TransferCounts,
    /// The server repo didn't exist and this push created it. The caller
    /// offers to land the branch on main once the checkout is linked (see
    /// [`offer_land_on_main_after_first_push`]).
    repo_created: bool,
}

const REMOTE_OVERRIDE_RECONCILIATION_NOTE: &str = "the mutation used a remote override that is not the checkout's configured remote, so `oak branch show --remote` would inspect a different server; `oak info --json` only identifies the configured local context";

fn shell_quote_recommendation_arg(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

pub(super) fn publication_branch_reconciliation_command(branch: &str) -> String {
    let quoted = shell_quote_recommendation_arg(branch);
    if quoted == branch {
        format!("oak branch show {branch} --remote --json")
    } else {
        format!("oak branch show --remote --json -- {quoted}")
    }
}

fn publication_reconciliation_context(
    repo: &dyn Repository,
    remote: &str,
    branch_name: Option<&str>,
) -> Result<(Vec<String>, Option<&'static str>)> {
    let actual_remote = normalize_remote_url(remote);
    let configured_remote = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .and_then(|value| normalize_remote_url(&value));
    if actual_remote != configured_remote {
        return Ok((
            vec!["oak info --json".to_string()],
            Some(REMOTE_OVERRIDE_RECONCILIATION_NOTE),
        ));
    }
    Ok((
        branch_name
            .map(|branch| vec![publication_branch_reconciliation_command(branch)])
            .unwrap_or_else(|| vec!["oak info --json".to_string()]),
        None,
    ))
}

fn publication_reason(reason: impl Into<String>, note: Option<&str>) -> String {
    let reason = reason.into();
    note.map_or(reason.clone(), |note| format!("{reason}; {note}"))
}

fn annotate_publication_error(error: OakError, note: Option<&str>) -> OakError {
    match (error, note) {
        (
            OakError::PublicationUnconfirmed {
                operation,
                reason,
                reconciliation_commands,
            },
            Some(note),
        ) => OakError::PublicationUnconfirmed {
            operation,
            reason: publication_reason(reason, Some(note)),
            reconciliation_commands,
        },
        (error, _) => error,
    }
}

fn validate_successful_publication_ack(
    response: &PushResponse,
    submitted_target: Option<&Hash>,
    operation: &str,
    reconciliation_commands: &[String],
) -> Result<()> {
    if !response.success {
        return Ok(());
    }
    let Some(acknowledged) = response.new_head.as_deref() else {
        // Released servers use null for compatible metadata-only and legacy
        // success receipts. It confirms the operation-level response but is
        // not independent exact-head evidence.
        return Ok(());
    };
    let native_hash = acknowledged.len() == 64
        && acknowledged
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    let matches_target = native_hash
        && Hash::from_hex(acknowledged)
            .ok()
            .is_some_and(|acknowledged| {
                submitted_target.is_none_or(|target| acknowledged == *target)
            });
    if !matches_target {
        return Err(crate::http::publication_unconfirmed(
            operation,
            "the acknowledged head did not match the submitted target",
            reconciliation_commands.to_vec(),
        ));
    }
    Ok(())
}

fn publication_identity(endpoint_path: &str) -> Option<(&str, &str)> {
    let (owner, repo) = endpoint_path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then_some((owner, repo))
}

struct PublicationAttemptInput<'a> {
    branch: Option<&'a BranchData>,
    target: Option<&'a Hash>,
    expected: Option<&'a Hash>,
    force: bool,
    transport: crate::publication_state::PublicationTransport,
    body: &'a [u8],
    request_url: &'a str,
    final_handoff: Option<&'a StagedFinalHandoff>,
}

fn prepare_publication_attempt(
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    input: PublicationAttemptInput<'_>,
) -> Result<Option<crate::publication_state::AttemptHandle>> {
    let (Some(branch), Some(target), Some((owner, repo))) = (
        input.branch,
        input.target,
        publication_identity(endpoint_path),
    ) else {
        return Ok(None);
    };
    let desired = crate::publication_state::DesiredBranchState {
        head: target.to_string(),
        description: branch.description.clone(),
        parent_branch: branch.parent_branch.clone(),
        status: branch.status.clone(),
        close_reason: branch.close_reason.clone(),
    };
    let expected_predecessor = if input.force {
        crate::publication_state::ExpectedPredecessor::Unconstrained
    } else if let Some(expected) = input.expected {
        crate::publication_state::ExpectedPredecessor::Exact(expected.to_string())
    } else {
        crate::publication_state::ExpectedPredecessor::Absent
    };
    let request_url = reqwest::Url::parse(input.request_url).map_err(|_| {
        OakError::InvalidArgument(
            "could not record the publication request path; no remote state was mutated"
                .to_string(),
        )
    })?;
    let store = match crate::publication_state::store_for_work_tree(work_tree) {
        Ok(store) => store,
        // This low-level helper is also used with standalone in-memory/test
        // repositories that have neither a checkout nor a registered mount.
        // Public CLI checkout and mount callers always have one of the two
        // durable adapters; keep the older embedding seam operational without
        // claiming restart coverage for it.
        Err(OakError::RepoNotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut attempt = store.reserve(crate::publication_state::NewAttempt {
        remote,
        owner,
        repo,
        branch: &branch.name,
        expected_predecessor,
        target_head: target.as_str(),
        desired_branch: &desired,
        transport: input.transport,
        request_body: input.body,
        endpoint_path: request_url.path(),
    })?;
    attempt.mark_sent()?;
    if let Some(handoff) = input.final_handoff {
        handoff.record(attempt.operation_id());
    }
    Ok(Some(attempt))
}

fn publication_error_with_operation(
    error: OakError,
    attempt: Option<&crate::publication_state::AttemptHandle>,
) -> OakError {
    let Some(attempt) = attempt else { return error };
    match error {
        OakError::PublicationUnconfirmed {
            operation,
            reason,
            mut reconciliation_commands,
        } => {
            reconciliation_commands
                .insert(0, "oak agent state --refresh --json --compact".to_string());
            OakError::PublicationUnconfirmed {
                operation,
                reason: format!(
                    "{reason}; durable local operation id {}",
                    attempt.operation_id()
                ),
                reconciliation_commands,
            }
        }
        error => error,
    }
}

fn reject_and_retire_publication_attempt(
    attempt: &mut Option<crate::publication_state::AttemptHandle>,
) {
    if let Some(mut handle) = attempt.take() {
        if handle.mark_rejected().is_ok() {
            let _ = handle.retire();
        }
    }
}

fn acknowledge_and_finish_publication_attempt(
    attempt: &mut Option<crate::publication_state::AttemptHandle>,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    target: Option<&Hash>,
) -> Result<()> {
    let Some(mut handle) = attempt.take() else {
        return Ok(());
    };
    handle.mark_acknowledged().map_err(|error| {
        OakError::InvalidArgument(format!(
            "publication acknowledgement was valid, but durable operation {} could not record it: {error}; do not retry the mutation; inspect `oak agent state --refresh --json --compact`",
            handle.operation_id()
        ))
    })?;
    let (owner, repo_name) = publication_identity(endpoint_path).ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "publication acknowledgement was valid, but durable operation {} has no repository-shaped receipt destination; do not retry the mutation",
            handle.operation_id()
        ))
    })?;
    let branch = branch_name.ok_or_else(|| {
        OakError::InvalidArgument(
            "acknowledged branch-head publication lost its branch identity".to_string(),
        )
    })?;
    let target = target.ok_or_else(|| {
        OakError::InvalidArgument(
            "acknowledged branch-head publication lost its target identity".to_string(),
        )
    })?;
    crate::publication_state::persist_downstream_receipt(
        work_tree,
        remote,
        owner,
        repo_name,
        branch,
        target.as_str(),
    )
    .map_err(|error| {
        OakError::InvalidArgument(format!(
            "publication acknowledgement was valid, but its downstream receipt could not be saved for durable operation {}: {error}; do not retry the mutation; inspect `oak agent state --refresh --json --compact`",
            handle.operation_id()
        ))
    })?;
    let operation_id = handle.operation_id().to_string();
    handle.retire().map_err(|error| {
        OakError::InvalidArgument(format!(
            "publication acknowledgement and downstream receipt were saved, but durable operation {operation_id} could not be retired: {error}; do not retry the mutation; inspect `oak agent state --refresh --json --compact`"
        ))
    })
}

/// Push commits to remote server. When the remote answers with a redirect
/// to a trusted Oak host (the old origin has moved), the repo's stored
/// remote is updated and the push retried once against the new origin —
/// see `follow_remote_move`. An untrusted target surfaces as the
/// `RemoteMoved` error's "re-run with `-r`" message instead.
pub async fn run(
    path: &Path,
    remote_url: Option<&str>,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<()> {
    if let Some(ctx) = super::git_remote::git_backend(path) {
        return super::git_remote::push(&ctx, remote_url, force, false);
    }
    let remote = resolve_push_remote(path, remote_url)?;
    run_resolved_with_outcome(path, &remote.url, remote.persist, force, repo_spec).await?;
    Ok(())
}

/// Publication evidence for multi-step commands such as `start`. A successful
/// call alone is not proof that a branch has a remotely readable head.
pub(crate) async fn run_for_start(path: &Path) -> Result<Option<String>> {
    let remote = resolve_push_remote(path, None)?;
    Ok(
        run_resolved_with_outcome(path, &remote.url, remote.persist, false, None)
            .await?
            .current_branch_pushed_head,
    )
}

/// `oak push`: [`run`], then say whether the branch just published conflicts
/// with its parent. Background pushers keep plain [`run`]: nobody is there to
/// act on the answer, and the check spends review quota.
pub async fn run_with_merge_check(
    path: &Path,
    remote_url: Option<&str>,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<()> {
    if let Some(ctx) = super::git_remote::git_backend(path) {
        return super::git_remote::push(&ctx, remote_url, force, false);
    }
    let remote = resolve_push_remote(path, remote_url)?;
    let outcome =
        run_resolved_with_outcome(path, &remote.url, remote.persist, force, repo_spec).await?;
    if let Some(check) = merge_check_for_outcome(path, &outcome).await {
        check.report(&outcome.branch);
    }
    Ok(())
}

/// The post-push verdict for a checkout push, or `None` when there is nothing
/// to judge.
async fn merge_check_for_outcome(
    path: &Path,
    outcome: &PushCommandOutcome,
) -> Option<MergeCheckJson> {
    merge_check_for_head(
        path,
        &outcome.remote,
        &outcome.owner,
        &outcome.repo_name,
        &outcome.branch,
        outcome.current_branch_pushed_head.as_deref()?,
    )
    .await
}

/// The post-push verdict for `branch` at `head`, authenticated the way a push
/// from the checkout at `path` is. Advisory: see [`super::merge_check`].
pub(crate) async fn merge_check_for_head(
    path: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch: &str,
    head: &str,
) -> Option<MergeCheckJson> {
    let db_path = crate::resolve::resolve(path).ok()?.db_path().ok()?;
    let repo = SqliteRepository::open(&db_path).ok()?;
    let target = merge_check::merge_target(&repo, branch)?;
    let token = super::credentials::effective_token_for_repository(remote, &repo);
    Some(
        merge_check::check(MergeCheckTarget {
            remote_url: remote,
            owner,
            repo_name,
            token,
            branch,
            target: &target,
            head,
        })
        .await,
    )
}

/// Structured wrapper over the ordinary push protocol. The protocol and
/// old-server compatibility remain exactly those of [`run`]; this suppresses
/// narration and emits one append-only outcome only after publication
/// succeeds. `pushed_head` is the immutable local head included in that
/// successful publication request, so callers do not need a second status
/// parse to identify what was pushed.
pub async fn run_json(
    path: &Path,
    remote_url: Option<&str>,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<()> {
    if let Some(ctx) = super::git_remote::git_backend(path) {
        return super::git_remote::push(&ctx, remote_url, force, true);
    }
    let remote = resolve_push_remote(path, remote_url)?;
    // Resolve every local preflight before capture begins. If resolution
    // fails, main can still emit the single JSON error envelope; starting a
    // capture first would swallow that envelope when `?` returns early.
    output::begin_capture();
    let result =
        run_resolved_with_outcome(path, &remote.url, remote.persist, force, repo_spec).await;
    let _narration = output::end_capture();
    let outcome = result?;
    let check = merge_check_for_outcome(path, &outcome).await;
    print_push_receipt(outcome, check)
}

/// Emit the `oak push --json` receipt for a successful publication. Shared by
/// the checkout path and mounts so both report the same contract.
fn print_push_receipt(
    outcome: PushCommandOutcome,
    merge_check: Option<MergeCheckJson>,
) -> Result<()> {
    let review_url = super::branch_web_url(&outcome.remote, &outcome.repo, &outcome.branch);
    // Conflicts come first. Branch pushes don't run CI (it runs on main
    // after the merge), so there is no CI step to point at here.
    let mut recommended_next_commands = merge_check
        .as_ref()
        .filter(|check| check.has_conflicts())
        .map(|check| check.recommended_next_commands.clone())
        .unwrap_or_default();
    recommended_next_commands.push(format!(
        "oak branch review {} --remote --merge-preview --json",
        outcome.branch
    ));

    output::print_json(&PushJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        pushed: outcome.published,
        published: outcome.published,
        remote_contacted: true,
        remote: outcome.remote,
        repo: outcome.repo,
        branch: outcome.branch.clone(),
        pushed_head: outcome
            .published
            .then(|| outcome.current_branch_pushed_head.clone())
            .flatten(),
        current_branch_pushed_head: outcome.current_branch_pushed_head,
        current_branch_push_checked: true,
        review_url,
        merge_check,
        transfer: outcome.transfer,
        recommended_next_commands,
    })
}

/// `oak push --json` for a repository the caller already resolved (a mount's
/// cache db). Narration is captured off stdout; on success the same receipt
/// as [`run_json`] is printed and the published head is returned so the
/// caller can record it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn push_json_for_repo(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch: &str,
    merge_target: &str,
    api_key: Option<&str>,
) -> Result<Option<Hash>> {
    output::begin_capture();
    let result = push_async_with_outcome(
        repo,
        work_tree,
        remote,
        owner,
        repo_name,
        Some(branch),
        false,
        api_key,
    )
    .await;
    let _narration = output::end_capture();
    let publication = result?;
    let head = publication.current_branch_pushed_head.clone();
    let check = match head.as_ref() {
        Some(head) => Some(
            merge_check::check(MergeCheckTarget {
                remote_url: remote,
                owner,
                repo_name,
                token: api_key.map(ToString::to_string),
                branch,
                target: merge_target,
                head: head.as_str(),
            })
            .await,
        ),
        None => None,
    };
    print_push_receipt(
        PushCommandOutcome {
            remote: remote.to_string(),
            repo: format!("{owner}/{repo_name}"),
            owner: owner.to_string(),
            repo_name: repo_name.to_string(),
            branch: branch.to_string(),
            published: publication.published,
            current_branch_pushed_head: head.as_ref().map(ToString::to_string),
            transfer: publication.transfer,
        },
        check,
    )?;
    Ok(head)
}

/// Most commits / missing-blob hashes listed individually in a push plan.
const PUSH_PLAN_LIST_LIMIT: usize = 1000;

/// One outgoing commit in a push plan, in wire (parent-before-child) order.
#[derive(Debug, serde::Serialize)]
pub struct PushPlanCommitJson {
    pub hash: String,
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_parent: Option<String>,
    pub file_changes: usize,
}

/// Something the plan could not determine, and why.
#[derive(Debug, serde::Serialize)]
pub struct PushPlanIncompleteJson {
    pub reason: &'static str,
    pub detail: String,
}

/// `oak push --plan --json` (fb-471). Read-only: bound to `local_head` and
/// the observed server heads of `remote`/`repo`; any change to either makes
/// the plan stale. Append-only within `schema_version`.
#[derive(Debug, serde::Serialize)]
pub struct PushPlanJson {
    pub schema_version: u32,
    pub plan: bool,
    /// Always false: planning sends only read-only requests.
    pub mutated: bool,
    pub remote: String,
    pub repo: String,
    pub branch: String,
    pub local_head: Option<String>,
    /// The server's head for this branch when planned (null: none yet).
    pub remote_head: Option<String>,
    /// The server's repository head (`main`) when planned.
    pub repo_head: Option<String>,
    /// The repository does not exist on the server yet; push creates it.
    pub repo_will_be_created: bool,
    pub force: bool,
    /// True when every field below was determined; see `incomplete`.
    pub plan_complete: bool,
    /// True when push would publish nothing (the server already has
    /// `local_head` as this branch's head).
    pub up_to_date: bool,
    pub commit_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<PushPlanCommitJson>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub commits_truncated: bool,
    pub tree_count: Option<usize>,
    pub blob_count: Option<usize>,
    pub declared_blob_bytes: Option<u64>,
    /// Outgoing blobs the server reported it already has (read-only
    /// `blobs/check` probe without the verified-receipt predicate); null when
    /// the probe was not possible. This is server PRESENCE, not an upload
    /// estimate: push sends every blob inline for small operations (it skips
    /// the check below its byte threshold), and the staged protocol also
    /// re-uploads present blobs that lack verified receipts. See
    /// `blob_fields_semantics`.
    pub blobs_server_has: Option<usize>,
    pub blobs_missing_on_server_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blobs_missing_on_server: Vec<String>,
    /// Always `"server_presence_not_upload_set"`: names what the blob
    /// fields above measure so callers do not read them as bytes to send.
    pub blob_fields_semantics: &'static str,
    /// Outgoing blobs absent from the local store.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_local_blobs: Vec<String>,
    /// How push would publish this exact plan against the server's current
    /// capabilities: `ordinary` (one request), `ordinary_blob_preload`
    /// (new blobs uploaded in `preload_requests` headless requests, then one
    /// ordinary publication), `phase_one_bootstrap`, `staged_v1`,
    /// `staged_window_v1`, or `unavailable` (see `publication_blocked`).
    /// Absent when it could not be determined (see `incomplete`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<&'static str>,
    /// Headless blob-only requests push would send before the publication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preload_requests: Option<usize>,
    /// Present when push would refuse this plan before mutating anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_blocked: Option<PushPlanBlockedJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<PushPlanIncompleteJson>,
    pub recommended_next_commands: Vec<String>,
}

/// Why `oak push` would refuse the planned operation, with the same stable
/// code and message its error envelope carries.
#[derive(Debug, serde::Serialize)]
pub struct PushPlanBlockedJson {
    pub code: &'static str,
    pub message: String,
}

/// `oak push --plan --json`: run push's own boundary resolution and outgoing
/// planner, plus read-only presence probes, and stop before the first
/// mutating request. Never persists a remote, links a repository, creates
/// one, re-parents, or writes local state.
pub async fn plan_json(path: &Path, remote_url: Option<&str>, force: bool) -> Result<()> {
    if super::git_remote::git_backend(path).is_some() {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "push --plan",
        });
    }
    output::route_narration_to_stderr();
    let remote = resolve_push_remote(path, remote_url)?;
    let remote_url = remote.url;
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let branch = repo
        .get_current_branch_name()?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?;
    let (Some(owner), Some(repo_name)) = (owner, repo_name) else {
        return Err(OakError::InvalidArgument(
            "`oak push --plan` needs a linked repository (clone one, or publish once with `oak push --repo <org>/<repo>`); it never links or creates one"
                .to_string(),
        ));
    };
    let api_key = super::credentials::effective_token_for_repository(&remote_url, &repo);
    plan_json_for_repo(
        &repo,
        &remote_url,
        &owner,
        &repo_name,
        &branch,
        api_key.as_deref(),
        force,
    )
    .await
}

/// The read-only planner behind `oak push --plan --json`, for a repository,
/// identity and branch the caller already resolved. Mounts call this with
/// their cache db and virtual branch.
pub(crate) async fn plan_json_for_repo(
    repo: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    branch: &str,
    api_key: Option<&str>,
    force: bool,
) -> Result<()> {
    output::route_narration_to_stderr();
    let branch = branch.to_string();
    let endpoint_path = format!("{owner}/{repo_name}");
    let client = crate::http::api_client();
    let local_head = repo.get_branch_head(&branch)?;

    let observed =
        fetch_push_boundaries(&client, remote_url, &endpoint_path, Some(&branch), api_key).await?;
    let boundaries = observed.boundaries.clone();

    let mut plan = PushPlanJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        plan: true,
        mutated: false,
        remote: crate::http::redact_url_userinfo(remote_url),
        repo: endpoint_path.clone(),
        branch: branch.clone(),
        local_head: local_head.as_ref().map(ToString::to_string),
        remote_head: boundaries.remote_head.as_ref().map(ToString::to_string),
        repo_head: boundaries.repo_head.as_ref().map(ToString::to_string),
        repo_will_be_created: observed.repo_needs_creation,
        force,
        plan_complete: false,
        up_to_date: false,
        commit_count: None,
        commits: Vec::new(),
        commits_truncated: false,
        tree_count: None,
        blob_count: None,
        declared_blob_bytes: None,
        blobs_server_has: None,
        blobs_missing_on_server_count: None,
        blobs_missing_on_server: Vec::new(),
        blob_fields_semantics: "server_presence_not_upload_set",
        missing_local_blobs: Vec::new(),
        transport: None,
        preload_requests: None,
        publication_blocked: None,
        incomplete: Vec::new(),
        recommended_next_commands: vec!["oak push --json".to_string()],
    };

    // Push paths this read-only plan does not model are reported, not
    // guessed: each would plan differently (or mutate) before sending.
    let bootstraps_main =
        boundaries.repo_head.is_none() && push_needs_main_bootstrap(repo, &branch)?;
    if bootstraps_main {
        plan.incomplete.push(PushPlanIncompleteJson {
            reason: "bootstrap_main",
            detail: "the server repository has no main; push first bootstraps main from local history as a separate operation".to_string(),
        });
        return output::print_json(&plan);
    }
    if !force {
        if let Some(remote_head) = boundaries.remote_head.as_ref() {
            if push_target_diverged(repo, Some(&branch), remote_head)? {
                plan.incomplete.push(PushPlanIncompleteJson {
                    reason: "diverged",
                    detail: format!(
                        "the server branch head {remote_head} is not in local history; push would first re-parent (or ask for `oak pull`), so the outgoing commits are not known yet"
                    ),
                });
                plan.recommended_next_commands =
                    vec!["oak pull --json".to_string(), "oak push --json".to_string()];
                return output::print_json(&plan);
            }
        }
    }

    let no_server_heads = boundaries.repo_head.is_none() && boundaries.remote_head.is_none();
    let commits = plan_ordinary_branch_commits(
        repo,
        &client,
        remote_url,
        &endpoint_path,
        api_key,
        &branch,
        boundaries,
        None,
        force,
    )
    .await?;
    let refs: Vec<&oak_core::Commit> = commits.iter().collect();
    let objects = collect_planned_objects(repo, &refs)?;

    plan.up_to_date = commits.is_empty();
    plan.commit_count = Some(commits.len());
    plan.commits_truncated = commits.len() > PUSH_PLAN_LIST_LIMIT;
    plan.commits = commits
        .iter()
        .take(PUSH_PLAN_LIST_LIMIT)
        .map(|commit| PushPlanCommitJson {
            hash: commit.hash.to_string(),
            parent: commit.parent_hash.as_ref().map(ToString::to_string),
            merge_parent: commit.merge_parent_hash.as_ref().map(ToString::to_string),
            file_changes: commit.files.len(),
        })
        .collect();
    plan.tree_count = Some(objects.tree_hashes.len());
    plan.blob_count = Some(objects.blobs.len());
    plan.declared_blob_bytes = Some(
        objects
            .blobs
            .iter()
            .fold(0u64, |total, blob| total.saturating_add(blob.size)),
    );
    let mut missing_local: Vec<String> = objects
        .missing_blobs
        .iter()
        .map(ToString::to_string)
        .collect();
    missing_local.sort();
    plan.missing_local_blobs = missing_local;

    let mut remote_missing: Option<std::collections::HashSet<Hash>> = None;
    if objects.blobs.is_empty() {
        plan.blobs_server_has = Some(0);
        plan.blobs_missing_on_server_count = Some(0);
        remote_missing = Some(std::collections::HashSet::new());
    } else if observed.repo_needs_creation {
        plan.blobs_server_has = Some(0);
        plan.blobs_missing_on_server_count = Some(objects.blobs.len());
        remote_missing = Some(objects.blobs.iter().map(|blob| blob.hash.clone()).collect());
    } else {
        match remote_missing_staged_blobs(
            &client,
            remote_url,
            &endpoint_path,
            api_key,
            &objects.blobs,
            false,
        )
        .await
        {
            Ok(missing) => {
                let mut hashes: Vec<String> = missing.iter().map(ToString::to_string).collect();
                hashes.sort();
                plan.blobs_server_has = Some(objects.blobs.len().saturating_sub(missing.len()));
                plan.blobs_missing_on_server_count = Some(missing.len());
                hashes.truncate(PUSH_PLAN_LIST_LIMIT);
                plan.blobs_missing_on_server = hashes;
                remote_missing = Some(missing);
            }
            Err(error) => plan.incomplete.push(PushPlanIncompleteJson {
                reason: "blob_presence_unknown",
                detail: error.to_string(),
            }),
        }
    }
    // The transport push would select for exactly these totals, from the
    // same read-only capability probe. A plan that push would refuse must
    // say so instead of recommending the push.
    if let (Some(missing), false) = (remote_missing.as_ref(), commits.is_empty()) {
        let branch_row = repo
            .get_branch(&branch)?
            .ok_or_else(|| OakError::BranchNotFound(branch.clone()))?;
        let totals = push_operation_totals(&refs, &objects, &[branch_to_push_data(&branch_row)]);
        let selected = if staged_protocol_required(totals) {
            select_push_transport_for_missing(
                &client,
                remote_url,
                api_key,
                totals,
                &objects,
                missing,
                FirstPublicationState {
                    repo_needs_creation: observed.repo_needs_creation,
                    phase_one_ordinary_allowed: no_server_heads
                        && outgoing_operation_is_self_contained(&commits),
                    // The same exclusion push applies. A plan that needs a
                    // `main` bootstrap has already returned `bootstrap_main`
                    // above, so it can never be labelled a preload.
                    blob_preload_excluded: bootstraps_main
                        .then_some(MAIN_BOOTSTRAP_PRELOAD_EXCLUSION),
                },
            )
            .await
        } else {
            Ok(PushTransportDecision::ordinary(totals))
        };
        match selected {
            Ok(decision) => {
                plan.transport = Some(decision.label());
                match decision.transport {
                    PushTransport::OrdinaryBlobPreload => {
                        plan.preload_requests = Some(decision.preload_batches.len());
                    }
                    PushTransport::StagedRequiredUnavailable => {
                        plan.publication_blocked = Some(PushPlanBlockedJson {
                            code: "push_capability_unavailable",
                            message: decision.unavailable_error().to_string(),
                        });
                        plan.recommended_next_commands = Vec::new();
                    }
                    _ => {}
                }
            }
            Err(error) => plan.incomplete.push(PushPlanIncompleteJson {
                reason: "transport_unknown",
                detail: error.to_string(),
            }),
        }
    }
    plan.plan_complete = plan.incomplete.is_empty();
    output::print_json(&plan)
}

pub(crate) async fn run_resolved(
    path: &Path,
    remote_url: &str,
    persist_remote: bool,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<()> {
    run_resolved_with_outcome(path, remote_url, persist_remote, force, repo_spec)
        .await
        .map(|_| ())
}

/// [`run_resolved`] plus the post-push verdict, for `oak commit --push`.
pub(crate) async fn run_resolved_checked(
    path: &Path,
    remote_url: &str,
    persist_remote: bool,
) -> Result<Option<MergeCheckJson>> {
    let outcome = run_resolved_with_outcome(path, remote_url, persist_remote, false, None).await?;
    Ok(merge_check_for_outcome(path, &outcome).await)
}

async fn run_resolved_with_outcome(
    path: &Path,
    remote_url: &str,
    persist_remote: bool,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<PushCommandOutcome> {
    let outcome = match run_once(path, remote_url, persist_remote, force, repo_spec).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            if persist_remote {
                super::follow_remote_move(path, remote_url, &origin)?;
            } else {
                output::info(&format!(
                    "Remote {remote_url} has moved to {origin} — retrying for this command"
                ));
            }
            run_once(path, &origin, persist_remote, force, repo_spec).await
        }
        result => result,
    }?;

    // Publication succeeded. Cache only the exact branch/head that the push
    // implementation returned, never a later database reread. A local commit,
    // branch switch, remote-configuration change, or `--refresh` invalidates
    // or supersedes this receipt naturally.
    if persist_remote {
        if let (Ok(ctx), Some(head)) = (
            crate::resolve::resolve(path),
            outcome.current_branch_pushed_head.as_deref(),
        ) {
            if let Err(error) = crate::work_state::record_checkout_push_success(
                &ctx.oak_dir,
                &outcome.remote,
                &outcome.owner,
                &outcome.repo_name,
                &outcome.branch,
                head,
            ) {
                output::vlog(&format!(
                    "push: could not persist local pushed-head receipt: {error}"
                ));
            }
        }
    }
    Ok(outcome)
}

pub(crate) fn resolve_push_remote(
    path: &Path,
    explicit_remote: Option<&str>,
) -> Result<ResolvedPushRemote> {
    if let Some(remote) = explicit_remote {
        let Some(remote) = normalize_remote_url(remote) else {
            return Err(OakError::InvalidArgument(
                "remote URL cannot be empty".to_string(),
            ));
        };
        return Ok(ResolvedPushRemote {
            url: remote,
            persist: true,
            source: PushRemoteSource::Explicit,
        });
    }

    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let stored_remote = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .and_then(|remote| normalize_remote_url(&remote));

    if let Some(remote) = env_remote_override() {
        return Ok(ResolvedPushRemote {
            url: remote,
            persist: stored_remote.is_none(),
            source: PushRemoteSource::Env,
        });
    }

    if let Some(remote) = stored_remote {
        return Ok(ResolvedPushRemote {
            url: remote,
            persist: true,
            source: PushRemoteSource::Stored,
        });
    }

    Ok(ResolvedPushRemote {
        url: DEFAULT_REMOTE.to_string(),
        persist: true,
        source: PushRemoteSource::Default,
    })
}

async fn run_once(
    path: &Path,
    remote_url: &str,
    persist_remote: bool,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<PushCommandOutcome> {
    let ctx = crate::resolve::resolve(path)?;
    let db_path = ctx.db_path()?;
    let work_tree = ctx.work_tree.clone();
    let repo = SqliteRepository::open(&db_path)?;

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    // Get API key from env var, per-repo metadata, or global credentials
    let api_key = super::credentials::effective_token_for_repository(remote_url, &repo);

    let owner_meta = repo.get_metadata(MetadataKey::RepoOwner)?;
    let was_linked = owner_meta.is_some();
    let (owner, repo_name) = if let Some(owner) = owner_meta {
        let name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
            OakError::Config(
                "Repository metadata missing repo name; re-clone with `oak clone <org>/<repo>`"
                    .to_string(),
            )
        })?;
        (owner, name)
    } else if let Some(spec) = repo_spec {
        // `--repo org/repo` (or OAK_REPO): link non-interactively. The repo
        // is auto-created on the server when the push 404s; ORG must be an
        // existing org slug.
        parse_remote_identity(spec)?
    } else {
        setup_remote_identity(&repo, remote_url, api_key.as_deref()).await?
    };
    let publication = push_async_with_outcome(
        &repo,
        &work_tree,
        remote_url,
        &owner,
        &repo_name,
        branch_name.as_deref(),
        force,
        api_key.as_deref(),
    )
    .await?;
    // A requested destination is provisional until the push succeeds. Failed
    // first pushes and refused remote overrides must preserve existing linkage
    // (including the credential's original origin binding).
    if persist_remote {
        super::credentials::set_repository_remote(&repo, remote_url)?;
    }
    if !was_linked {
        repo.set_metadata(MetadataKey::RepoOwner, &owner)?;
        repo.set_metadata(MetadataKey::RepoName, &repo_name)?;
        push_success(&format!("Linked to {owner}/{repo_name} on {remote_url}"));
    }
    // Only now is the link saved, which the merge behind this offer reads.
    // A remote override that isn't persisted would merge against the stored
    // remote instead of the one just pushed to, so skip the offer then.
    if publication.repo_created && persist_remote {
        if let Some(name) = branch_name.as_deref() {
            offer_land_on_main_after_first_push(&repo, &work_tree, name).await?;
        }
    }
    let branch =
        branch_name.ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    Ok(PushCommandOutcome {
        remote: remote_url.to_string(),
        repo: format!("{owner}/{repo_name}"),
        owner,
        repo_name,
        branch,
        published: publication.published,
        current_branch_pushed_head: publication
            .current_branch_pushed_head
            .map(|hash| hash.to_string()),
        transfer: publication.transfer,
    })
}

/// Async push implementation
#[allow(clippy::too_many_arguments)]
pub async fn push_async(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
) -> Result<()> {
    push_async_with_outcome(
        repo,
        work_tree,
        remote,
        owner,
        repo_name,
        branch_name,
        force,
        api_key,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn push_async_with_outcome(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
) -> Result<EndpointPushOutcome> {
    let repo_path = format!("{owner}/{repo_name}");
    push_async_with_endpoint_outcome(
        repo,
        work_tree,
        remote,
        &repo_path,
        branch_name,
        force,
        api_key,
        None,
        Some(&repo_path),
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn push_async_suppressing_result(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
) -> Result<()> {
    let repo_path = format!("{owner}/{repo_name}");
    push_async_with_endpoint(
        repo,
        work_tree,
        remote,
        &repo_path,
        branch_name,
        force,
        api_key,
        None,
        Some(&repo_path),
        false,
    )
    .await
}

fn acknowledge_published_description(
    repo: &dyn Repository,
    name: &str,
    description: Option<&str>,
    generation: Option<i64>,
) {
    if let Err(error) = repo.acknowledge_branch_description(name, description, generation) {
        output::warning(&format!("Description published, but local synchronization tracking could not be saved: {error}. A later pull may retain the local description until it matches the remote."));
    }
}

/// Send a branch-metadata-only push — no commits, no blobs, no trees,
/// just the `BranchPushData` payload so the server upserts the branch
/// row with the current description / parent / status. Used by
/// `oak desc` (both the mount path and plain workdirs), where we want
/// the new description to land on the server immediately rather than
/// waiting for the next push with real commits — a commit-less
/// `oak push` returns "Already up to date" before sending the branch
/// row, so a desc-only change would otherwise never sync.
///
/// Returns `Ok(())` when the server accepts the update. The caller is
/// responsible for translating network / auth failures into a friendly
/// "saved locally, retry with `oak push`" message — we want the local
/// description change to be durable even if this round-trip fails.
pub async fn push_branch_metadata(
    repo: &dyn Repository,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let description_generation = repo.description_publication_generation()?;
    let branch = repo
        .get_branch(branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.to_string()))?;

    let push_req = PushRequest {
        expected_head: None,
        expected_branch_head: None,
        force: false,
        branch: Some(BranchData {
            name: branch.name,
            description: branch.description,
            parent_branch: branch.parent_branch,
            status: branch.status.to_string(),
            created_at: branch.created_at.to_rfc3339(),
            close_reason: branch.close_reason.as_ref().map(|r| r.as_str().to_string()),
        }),
        commits: Vec::new(),
        blobs: Vec::new(),
        trees: Vec::new(),
    };

    let client = crate::http::api_client();
    let (reconciliation_commands, reconciliation_note) =
        publication_reconciliation_context(repo, remote, Some(branch_name))?;
    let body = serde_json::to_vec(&push_req).map_err(|_| {
        OakError::InvalidArgument(
            "could not encode branch metadata publication; no remote state was mutated".to_string(),
        )
    })?;
    let request = with_auth(
        client
            .post(format!("{remote}/api/{owner}/{repo_name}/push"))
            .header("x-oak-user", super::commit::get_author())
            .header(reqwest::header::CONTENT_TYPE, "application/json"),
        api_key,
    );
    let resp = crate::http::send_publication_request(
        request,
        body,
        "branch metadata publication",
        &reconciliation_commands,
    )
    .await
    .map_err(|error| annotate_publication_error(error, reconciliation_note))?;

    if !resp.status().is_success() {
        let status = resp.status();
        if crate::http::publication_status_is_unconfirmed(status) {
            return Err(crate::http::publication_unconfirmed(
                "branch metadata publication",
                publication_reason(
                    format!("the server returned HTTP {status}"),
                    reconciliation_note,
                ),
                reconciliation_commands,
            ));
        }
        return Err(OakError::Server(format!(
            "Branch metadata publication was rejected with HTTP {status}; the response body was omitted because remote diagnostics may contain secrets"
        )));
    }
    // The push endpoint signals rejection as HTTP 200 + `success: false`
    // (e.g. the conflict check), so an OK status alone doesn't mean the
    // branch row landed.
    let receipt = crate::http::bounded_publication_body(resp)
        .await
        .map_err(|reason| {
            crate::http::publication_unconfirmed(
                "branch metadata publication",
                publication_reason(reason.to_string(), reconciliation_note),
                reconciliation_commands.clone(),
            )
        })?;
    let body: PushResponse = serde_json::from_slice(&receipt).map_err(|_| {
        crate::http::publication_unconfirmed(
            "branch metadata publication",
            publication_reason(
                "the successful response did not contain a valid receipt",
                reconciliation_note,
            ),
            reconciliation_commands.clone(),
        )
    })?;
    validate_successful_publication_ack(
        &body,
        None,
        "branch metadata publication",
        &reconciliation_commands,
    )
    .map_err(|error| annotate_publication_error(error, reconciliation_note))?;
    if !body.success {
        return Err(OakError::Server(
            "Branch metadata publication was explicitly rejected; the remote message was omitted because it may contain secrets"
                .to_string(),
        ));
    }
    if let Some(branch) = &push_req.branch {
        acknowledge_published_description(
            repo,
            &branch.name,
            branch.description.as_deref(),
            description_generation,
        );
    }
    Ok(())
}

/// Set a new branch's head to its already-published parent snapshot. This is
/// separate from object ingestion: commit identity includes its original branch.
#[allow(clippy::too_many_arguments)]
async fn publish_inherited_branch_head(
    repo: &dyn Repository,
    client: &reqwest::Client,
    remote: &str,
    endpoint: &str,
    name: &str,
    target: &Hash,
    expected_head: Option<&Hash>,
    api_key: Option<&str>,
) -> Result<()> {
    let description_generation = repo.description_publication_generation()?;
    let branch = repo
        .get_branch(name)?
        .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
    let parent = branch.parent_branch.as_deref().ok_or_else(|| {
        OakError::InvalidArgument(
            "publishing an inherited head requires a parent branch; no head publication was sent"
                .to_string(),
        )
    })?;
    let response = with_auth(client.get(format!("{remote}/api/capabilities")), api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| OakError::Http(error.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(OakError::InvalidArgument(format!(
            "initial branch-head publication capability is unavailable (HTTP {}); no head publication was sent. Metadata may already be synchronized; inspect the branch before retrying",
            response.status(),
        )));
    }
    let bytes = crate::http::bounded_publication_body(response)
        .await
        .map_err(|_| {
            OakError::InvalidArgument(
                "invalid branch-head capability response; no head publication was sent".to_string(),
            )
        })?;
    let capability: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        OakError::InvalidArgument(
            "invalid branch-head capability response; no head publication was sent".to_string(),
        )
    })?;
    if !capability.is_object() || capability["branch_seed_protocol"] != "parent_head_cas_v1" {
        return Err(OakError::InvalidArgument(
            "server does not advertise branch_seed_protocol=parent_head_cas_v1; the inherited branch head is not published. Metadata may already be synchronized. Retry `oak push --json` after server support is available, or publish an actual work checkpoint".to_string(),
        ));
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "name": name, "description": branch.description, "parent_branch": parent,
        "initial_head": target.as_str(), "expected_head": expected_head.map(Hash::as_str),
    }))?;
    let (reconciliation, note) = publication_reconciliation_context(repo, remote, Some(name))?;
    let request = with_auth(
        client
            .post(format!("{remote}/api/{endpoint}/branches"))
            .header(reqwest::header::CONTENT_TYPE, "application/json"),
        api_key,
    );
    let response = crate::http::send_publication_request(
        request,
        body,
        "initial branch-head publication",
        &reconciliation,
    )
    .await
    .map_err(|error| annotate_publication_error(error, note))?;
    let status = response.status();
    if !status.is_success() {
        if crate::http::publication_status_is_unconfirmed(status) {
            return Err(crate::http::publication_unconfirmed(
                "initial branch-head publication",
                format!("server returned HTTP {status}"),
                reconciliation,
            ));
        }
        return Err(OakError::InvalidArgument(format!(
            "initial branch-head publication was refused (HTTP {status}); inspect the branch and synchronize its parent before retrying. A moved/closed target is never overwritten; an absent or inaccessible endpoint is not proof of server support",
        )));
    }
    let bytes = crate::http::bounded_publication_body(response)
        .await
        .map_err(|reason| {
            crate::http::publication_unconfirmed(
                "initial branch-head publication",
                reason.to_string(),
                reconciliation.clone(),
            )
        })?;
    let receipt: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        crate::http::publication_unconfirmed(
            "initial branch-head publication",
            "invalid success receipt",
            reconciliation.clone(),
        )
    })?;
    if receipt["name"] != name
        || receipt["head"] != target.as_str()
        || receipt["parent_branch"] != parent
        || receipt["status"] != "open"
        || receipt.get("description") != Some(&serde_json::to_value(&branch.description)?)
    {
        return Err(crate::http::publication_unconfirmed("initial branch-head publication", "success receipt did not match the exact requested open branch, parent, head and description", reconciliation));
    }
    acknowledge_published_description(
        repo,
        name,
        branch.description.as_deref(),
        description_generation,
    );
    Ok(())
}

/// Batch size for the bootstrap loop that pushes imported git history.
/// Each batch is one round-trip and one round of in-memory blob/tree
/// collection — so the value trades request count vs. peak memory. 500
/// commits keeps the per-batch JSON body well under the 100 MB Cloudflare
/// request limit and bounds the blob working set for typical repos.
const BOOTSTRAP_BATCH_SIZE: usize = oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS;
const STAGED_MAX_TREES: usize = oak_core::protocol::STAGED_MAX_TREE_OBJECTS;
const STAGED_MAX_TREE_ENTRIES: usize = oak_core::protocol::STAGED_MAX_DIRECT_TREE_ENTRIES;
const STAGED_MAX_BLOBS: usize = oak_core::protocol::STAGED_MAX_BLOBS;
const STAGED_MAX_BLOB_BYTES: u64 = oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES;
const STAGED_CLIENT_BLOB_BATCH_BYTES: u64 = 64 * 1024 * 1024;
const STAGED_MAX_FILE_CHANGES: usize = oak_core::protocol::STAGED_MAX_FILE_CHANGES;
const STAGED_MAX_RESOLVED_ENTRIES: usize = oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES;
const STAGED_MAX_METADATA_BYTES: usize = oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES;

struct PreplannedObjects {
    trees: Vec<TreeData>,
    blobs: Vec<BlobData>,
    chunks: Vec<PreparedChunk>,
}

enum PreparedChunk {
    /// A small blob travelling as its own single chunk; its bytes are
    /// resident (it was loaded to be sent inline).
    Buffered(ChunkRef, Vec<u8>),
    /// A content-defined blob held in the local store. Its chunks are cut
    /// from there, one negotiation page at a time, while they upload.
    Blob { hash: Hash, size: u64 },
}

struct PreplannedBatch {
    window_budget: bool,
    stage_id: Option<String>,
    expected_head: Option<Hash>,
    commits: Vec<oak_core::Commit>,
    staged_protocol: bool,
    finalize: bool,
    target_head: Option<Hash>,
    objects: Option<PreplannedObjects>,
    final_handoff: Option<StagedFinalHandoff>,
    /// The operation's content-transfer session: every batch of one push
    /// shares its upload slots, observed counts and progress.
    transfer: Arc<TransferSession>,
}

#[derive(Clone, Default)]
struct StagedFinalHandoff(std::sync::Arc<std::sync::Mutex<Option<String>>>);

impl StagedFinalHandoff {
    fn record(&self, operation_id: &str) {
        *self.0.lock().expect("staged final handoff lock poisoned") =
            Some(operation_id.to_string());
    }

    fn clear(&self) {
        *self.0.lock().expect("staged final handoff lock poisoned") = None;
    }

    fn operation_id(&self) -> Option<String> {
        self.0
            .lock()
            .expect("staged final handoff lock poisoned")
            .clone()
    }
}

/// Async push implementation that works with both repo and space endpoints.
/// `endpoint_path` is e.g. "repos/my-repo".
///
/// `preplanned_batch` is one already-admitted, topologically ordered slice of
/// a larger bootstrap operation plus the exact remote head it was planned to
/// extend. Normal pushes pass `None` and construct their outgoing plan after
/// remote-head discovery.
///
/// `repo_path_for_web` is `Some("owner/repo")` only for repo-shaped web UI
/// routes. Non-repo API endpoints should pass `None` so the CLI does not
/// pretend every push endpoint maps to `/<owner>/<repo>/branches/<branch>`.
#[allow(clippy::too_many_arguments)]
async fn push_async_with_endpoint(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
    preplanned_batch: Option<PreplannedBatch>,
    repo_path_for_web: Option<&str>,
    emit_result: bool,
) -> Result<()> {
    push_async_with_endpoint_outcome(
        repo,
        work_tree,
        remote,
        endpoint_path,
        branch_name,
        force,
        api_key,
        preplanned_batch,
        repo_path_for_web,
        emit_result,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn push_async_with_endpoint_outcome(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
    mut preplanned_batch: Option<PreplannedBatch>,
    repo_path_for_web: Option<&str>,
    emit_result: bool,
) -> Result<EndpointPushOutcome> {
    let client = crate::http::api_client();
    // One content-transfer session per push operation: the top-level call
    // creates it, every preplanned batch of the operation shares it.
    let owns_transfer_session = preplanned_batch.is_none();
    let transfer_session = preplanned_batch
        .as_ref()
        .map(|batch| batch.transfer.clone())
        .unwrap_or_else(TransferSession::new);
    // The human summary of what was transferred, printed by the top-level
    // call just before its result line.
    let announce_transfer = || {
        if owns_transfer_session && transfer_session.counts().chunks_required > 0 {
            push_success(&transfer_session.summary());
        }
    };
    let finish = |published: bool, head: Option<Hash>| {
        if owns_transfer_session {
            transfer_session.finish_progress();
        }
        EndpointPushOutcome {
            published,
            current_branch_pushed_head: head,
            transfer: transfer_session.counts(),
            repo_created: false,
        }
    };

    // Get remote head — and, for a branch-scoped push, the branch head in
    // the same network window.
    // (Progress narration in this flow goes through push_info/push_success:
    // piped consumers get a single result line at the end instead.) The two GETs are independent reads and each
    // costs a full server round trip (~100-250ms against oak.space), so
    // sequencing them doubled the fixed latency of every push. A 404 from
    // the branch GET (repo or branch not on the server yet) means "no
    // remote head", same as before.
    // Tracks whether we auto-created the repo on this push. Used after
    // the push succeeds to prompt for a merge — a fresh repo with only a
    // feature-branch push looks empty in the web UI until `main` has a
    // commit, which reads like the push silently failed.
    let mut created_repo = false;
    let ObservedPushBoundaries {
        repo_needs_creation,
        boundaries: PushBoundaries {
            repo_head,
            remote_head,
        },
    } = fetch_push_boundaries(&client, remote, endpoint_path, branch_name, api_key).await?;
    let mut repo_needs_creation = repo_needs_creation;
    if let Some(name) = branch_name {
        require_stacked_parent_published(
            &client,
            repo,
            remote,
            endpoint_path,
            name,
            repo_needs_creation,
            api_key,
        )
        .await?;
    }
    if let Some(batch) = &preplanned_batch {
        if remote_head != batch.expected_head {
            return Err(OakError::RemoteCommitsNotInLocalHistory);
        }
    }

    // Before a potentially batched bootstrap publishes batch one, admit the
    // complete operation suffix (all main batches plus the target branch).
    // This is remote-boundary-specific, not a scan of unrelated local history.
    let mut planned_bootstrap_commits: Option<Vec<oak_core::Commit>> = None;
    let mut planned_bootstrap_start: Option<Option<Hash>> = None;
    let mut planned_bootstrap_objects: Option<PlannedObjects> = None;
    let mut planned_target_commits: Option<Vec<oak_core::Commit>> = None;
    if repo_head.is_none() {
        if let Some(name) = branch_name {
            if push_needs_main_bootstrap(repo, name)? {
                let server_main =
                    fetch_remote_branch_head(&client, remote, endpoint_path, "main", api_key)
                        .await?;
                let required_main = (!force).then_some(server_main.as_ref()).flatten();
                let main_external: std::collections::HashSet<String> =
                    server_main.iter().map(ToString::to_string).collect();
                let main_commits = plan_outgoing_commits(
                    repo,
                    "main",
                    repo.get_branch_head("main")?.as_ref(),
                    required_main,
                    &main_external,
                )?;

                // Direction is intentionally asymmetric: main may close only
                // over its proven server boundary. The target may additionally
                // depend on the just-admitted main plan.
                let mut target_external: std::collections::HashSet<String> = [
                    repo_head.as_ref(),
                    remote_head.as_ref(),
                    server_main.as_ref(),
                ]
                .into_iter()
                .flatten()
                .map(ToString::to_string)
                .collect();
                target_external.extend(main_commits.iter().map(|commit| commit.hash.to_string()));
                let required_branch = (!force).then_some(remote_head.as_ref()).flatten();
                let branch_commits = plan_target_with_remote_proofs(
                    repo,
                    &client,
                    remote,
                    endpoint_path,
                    api_key,
                    name,
                    repo.get_branch_head(name)?.as_ref(),
                    required_branch,
                    target_external,
                )
                .await?;
                let operation_commits: Vec<&oak_core::Commit> =
                    main_commits.iter().chain(&branch_commits).collect();
                let objects = collect_planned_objects(repo, &operation_commits)?;
                if !objects.missing_blobs.is_empty() {
                    let mut hashes: Vec<String> = objects
                        .missing_blobs
                        .iter()
                        .map(|hash| hash.to_string())
                        .collect();
                    hashes.sort();
                    return Err(OakError::InvalidArgument(format!(
                        "push admission phase is missing local blob(s) {} in the whole bootstrap operation; restore them before retrying. No remote state was mutated",
                        hashes.join(", ")
                    )));
                }
                // This combined plan proves closure only. Staged aggregate
                // caps are applied later to the exact session that uses
                // staged-v1; legacy ordinary pushes retain their deployed
                // compatibility envelope.
                // The combined descriptor plan above is admission-only. Do
                // not claim target-only immutable objects into main's staged
                // session: every session owns exactly the closure it will
                // finalize, while the target slice is re-read lazily later.
                drop(objects);
                let main_refs: Vec<&oak_core::Commit> = main_commits.iter().collect();
                let main_objects = collect_planned_objects(repo, &main_refs)?;
                planned_bootstrap_commits = Some(main_commits);
                planned_bootstrap_start = Some(server_main);
                planned_bootstrap_objects = Some(main_objects);
                planned_target_commits = Some(branch_commits);
            }
        }
    }

    // Bootstrap pass: when the server's repo is brand-new (empty `main`)
    // and the user is on a personal branch parented onto `main` whose
    // local main has imported commits, push those imported commits first
    // with `branch_name = "main"` on the wire. This is what makes
    // `oak clone <git-url>` + `oak push` and `oak init` (with git import)
    // + `oak push` actually transfer the imported history — without it
    // push would walk only the personal branch's own commits (zero right
    // after import) and report "already up to date" while abandoning the
    // imported history on the client.
    //
    // The server's bootstrap exception (`pushes_to_main` in
    // `oak-server/src/api/repos.rs`) requires `req.branch.name == "main"`
    // AND at least one commit with `branch_name == "main"`. Both hold
    // for the recursive call because `convert_history` writes imported
    // commits with `branch_name = "main"` and the recursive call sets
    // branch_data from the local `main` row.
    let mut bootstrapped_main = false;
    if repo_head.is_none() {
        if let Some(name) = branch_name {
            if name != "main" {
                let on_main_child = repo
                    .get_branch(name)?
                    .and_then(|b| b.parent_branch)
                    .is_some_and(|p| p == "main");
                let local_main_has_commits = repo.get_branch_head("main")?.is_some();
                if on_main_child && local_main_has_commits {
                    let main_commits = planned_bootstrap_commits.take().ok_or_else(|| {
                        OakError::InvalidArgument(
                            "push admission phase did not produce a bootstrap plan; no remote state was mutated"
                                .to_string(),
                        )
                    })?;
                    let total_main_commits = main_commits.len();
                    push_info(&format!(
                        "Bootstrapping `main` from imported history ({total_main_commits} commits, admitted as one operation)..."
                    ));

                    let pb = indicatif::ProgressBar::new(total_main_commits as u64);
                    pb.set_style(
                        indicatif::ProgressStyle::default_bar()
                            .template(
                                "  Bootstrap [{bar:30.magenta/dim}] {pos}/{len} commits ({elapsed_precise})",
                            )
                            .unwrap()
                            .progress_chars("━╸─"),
                    );

                    // Execute immutable slices from the one whole-operation
                    // plan. No batch re-sorts or re-walks the remaining
                    // history, so later corruption cannot hide behind batch 1.
                    let mut expected_head = planned_bootstrap_start.take().ok_or_else(|| {
                        OakError::InvalidArgument(
                            "push admission phase did not preserve the bootstrap boundary; no remote state was mutated"
                            .to_string(),
                        )
                    })?;
                    let objects = planned_bootstrap_objects.take().ok_or_else(|| {
                        OakError::InvalidArgument(
                            "push admission phase did not preserve the bootstrap object plan; no remote state was mutated"
                                .to_string(),
                        )
                    })?;
                    let main_refs: Vec<&oak_core::Commit> = main_commits.iter().collect();
                    let main_branch = repo
                        .get_branch("main")?
                        .ok_or_else(|| OakError::BranchNotFound("main".to_string()))?;
                    let phase_one_bootstrap = repo_head.is_none()
                        && expected_head.is_none()
                        && outgoing_operation_is_self_contained(&main_commits);
                    let decision = select_push_transport_for_plan(
                        &client,
                        remote,
                        endpoint_path,
                        api_key,
                        push_operation_totals(
                            &main_refs,
                            &objects,
                            &[branch_to_push_data(&main_branch)],
                        ),
                        &objects,
                        FirstPublicationState {
                            repo_needs_creation,
                            phase_one_ordinary_allowed: phase_one_bootstrap,
                            // `main` is bootstrapped only into a repository
                            // without one: never as a preload-then-publish.
                            blob_preload_excluded: Some(MAIN_BOOTSTRAP_PRELOAD_EXCLUSION),
                        },
                    )
                    .await?;
                    let transport = decision.transport;
                    if matches!(
                        transport,
                        PushTransport::StagedRequiredUnavailable
                            | PushTransport::OrdinaryBlobPreload
                    ) {
                        return Err(decision.unavailable_error());
                    }
                    if transport == PushTransport::PhaseOneOrdinaryBootstrap {
                        publish_phase_one_ordinary_bootstrap(
                            repo,
                            work_tree,
                            remote,
                            endpoint_path,
                            "main",
                            force,
                            api_key,
                            expected_head,
                            main_commits,
                            objects,
                            Some(&pb),
                            &transfer_session,
                        )
                        .await?;
                    } else if matches!(
                        transport,
                        PushTransport::StagedReady | PushTransport::StagedWindowReady
                    ) {
                        let main_tip = repo.get_branch_head("main")?.ok_or_else(|| {
                            OakError::InvalidArgument(
                                "push admission phase lost bootstrap main tip; no remote state was mutated"
                                    .to_string(),
                            )
                        })?;
                        publish_staged_plan(
                            repo,
                            work_tree,
                            remote,
                            endpoint_path,
                            "main",
                            force,
                            api_key,
                            repo_needs_creation,
                            transport == PushTransport::StagedWindowReady,
                            expected_head,
                            main_tip,
                            main_commits,
                            objects,
                            Some(&pb),
                            &transfer_session,
                        )
                        .await?;
                    } else {
                        for batch in main_commits.chunks(BOOTSTRAP_BATCH_SIZE) {
                            let batch = batch.to_vec();
                            let batch_len = batch.len();
                            let next_head = batch.last().map(|commit| commit.hash.clone());
                            Box::pin(push_async_with_endpoint(
                                repo,
                                work_tree,
                                remote,
                                endpoint_path,
                                Some("main"),
                                force,
                                api_key,
                                Some(PreplannedBatch {
                                    window_budget: false,
                                    stage_id: None,
                                    expected_head: expected_head.clone(),
                                    commits: batch,
                                    staged_protocol: false,
                                    finalize: false,
                                    target_head: None,
                                    objects: None,
                                    final_handoff: None,
                                    transfer: transfer_session.clone(),
                                }),
                                repo_path_for_web,
                                emit_result,
                            ))
                            .await?;
                            pb.inc(batch_len as u64);
                            expected_head = next_head;
                        }
                    }
                    pb.finish_and_clear();
                    push_success(&format!(
                        "Bootstrapped `main` with {total_main_commits} commits"
                    ));
                    bootstrapped_main = true;
                    // The recursive main push created the missing repo after
                    // admitting its first exact batch.
                    repo_needs_creation = false;
                }
            }
        }
    }

    // An empty task follows its parent without inventing a branch-owned
    // checkpoint. Publish that inherited transition through an explicit CAS,
    // before the outgoing-commit planner (there are no own commits to send).
    if preplanned_batch.is_none() && !force && !repo_needs_creation {
        if let Some(name) = branch_name.filter(|name| *name != oak_core::DEFAULT_BRANCH) {
            if repo.get_commits_for_branch(name)?.is_empty() {
                if let (Some(branch), Some(target)) =
                    (repo.get_branch(name)?, repo.get_branch_head(name)?)
                {
                    if let Some(parent) = branch.parent_branch.as_deref() {
                        if repo.get_branch_head(parent)?.as_ref() == Some(&target)
                            && remote_head.as_ref() != Some(&target)
                        {
                            publish_inherited_branch_head(
                                repo,
                                &client,
                                remote,
                                endpoint_path,
                                name,
                                &target,
                                remote_head.as_ref(),
                                api_key,
                            )
                            .await?;
                            return Ok(finish(true, Some(target)));
                        }
                    }
                }
            }
        }
    }

    // Check for divergent history. The server's branch head not being in
    // local history is most often NOT real divergence: the branch's
    // server-side seed moved because `main` advanced under it (no real
    // remote work). That case self-heals here — re-parent onto the moved
    // seed and continue the push in this same invocation (Invariant 3).
    // Only real foreign commits on the remote branch abort, with `oak pull`
    // (which now converges) as the single instruction.
    if !force {
        if let Some(ref rh) = remote_head {
            // For a branch push, divergence means the branch doesn't EXTEND
            // the server's head — ancestry, not mere presence. A full clone
            // holds every server commit (including the moved seed), so
            // `has_commit` alone would wave the push through to a guaranteed
            // server-side rejection. The repo-scoped fallback keeps the
            // legacy presence check.
            let diverged = push_target_diverged(repo, branch_name, rh)?;
            if diverged {
                self_heal_diverged_push(
                    repo,
                    work_tree,
                    remote,
                    endpoint_path,
                    branch_name,
                    api_key,
                )
                .await?;
            }
        }
    } else {
        output::warning("Force push: overwriting remote history");
    }

    // Build (or consume) the exact topological operation slice. Imported
    // timestamps are provenance, never execution order.
    let window_budget = preplanned_batch
        .as_ref()
        .is_some_and(|batch| batch.window_budget);
    let preplanned_control = preplanned_batch.as_ref().map(|batch| {
        (
            batch.stage_id.clone(),
            batch.staged_protocol,
            batch.finalize,
            batch.expected_head.clone(),
            batch.target_head.clone(),
            batch.final_handoff.clone(),
        )
    });
    let preplanned_objects = preplanned_batch
        .as_mut()
        .and_then(|batch| batch.objects.take());
    let had_preplanned_payload = preplanned_objects.is_some();
    let has_preplanned_objects = preplanned_objects
        .as_ref()
        .is_some_and(|objects| !objects.trees.is_empty() || !objects.blobs.is_empty());
    let commits = if let Some(batch) = preplanned_batch {
        batch.commits
    } else if let Some(commits) = planned_target_commits.take() {
        commits
    } else if let Some(name) = branch_name {
        let bootstrap_head = bootstrapped_main
            .then(|| repo.get_branch_head("main"))
            .transpose()?
            .flatten();
        plan_ordinary_branch_commits(
            repo,
            &client,
            remote,
            endpoint_path,
            api_key,
            name,
            PushBoundaries {
                repo_head: repo_head.clone(),
                remote_head: remote_head.clone(),
            },
            bootstrap_head.as_ref(),
            force,
        )
        .await?
    } else {
        // Fallback: get all commits
        repo.get_all_commits()?
    };

    if branch_name.is_none() {
        for commit in &commits {
            admit_commit_file_changes(repo, commit)?;
        }
    }

    // Any outgoing branch can exceed one safe publication envelope. Admit
    // its exact suffix once, then use an isolated staged session rather than
    // exposing a partial feature/main head or relying on timestamp slices.
    let server_heads: Vec<&Hash> = [remote_head.as_ref(), repo_head.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    let mut blob_preload_performed = false;
    let mut plan_admitted: std::collections::HashSet<Hash> = std::collections::HashSet::new();
    let server_known = if branch_name.is_some() && !commits.is_empty() {
        server_known_objects(
            repo,
            commits.iter().map(|commit| &commit.manifest_hash),
            &server_heads,
        )?
    } else {
        ServerKnownObjects::default()
    };
    if preplanned_control.is_none() && !commits.is_empty() {
        if let Some(name) = branch_name {
            let refs: Vec<&oak_core::Commit> = commits.iter().collect();
            let objects = collect_planned_objects_with_known(repo, &refs, Some(&server_known))?;
            // The planner streamed and hashed every blob the server is not
            // known to hold; collection below need not read them again.
            plan_admitted.extend(
                objects
                    .blobs
                    .iter()
                    .filter(|blob| !objects.server_has_blobs.contains(&blob.hash))
                    .map(|blob| blob.hash.clone()),
            );
            let branch = repo
                .get_branch(name)?
                .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
            let totals = push_operation_totals(&refs, &objects, &[branch_to_push_data(&branch)]);
            let phase_one_bootstrap = repo_head.is_none()
                && remote_head.is_none()
                && outgoing_operation_is_self_contained(&commits);
            let decision = select_push_transport_for_plan(
                &client,
                remote,
                endpoint_path,
                api_key,
                totals,
                &objects,
                FirstPublicationState {
                    repo_needs_creation,
                    phase_one_ordinary_allowed: phase_one_bootstrap,
                    blob_preload_excluded: None,
                },
            )
            .await?;
            let transport = decision.transport;
            if transport == PushTransport::StagedRequiredUnavailable {
                return Err(decision.unavailable_error());
            }
            if transport == PushTransport::OrdinaryBlobPreload {
                // Only immutable blobs leave this machine here. The commits,
                // trees, branch row and head still travel in the one ordinary
                // publication below, with its unchanged admission, CAS and
                // receipt handling.
                preload_blobs_headless(
                    &client,
                    repo,
                    remote,
                    endpoint_path,
                    api_key,
                    repo_head.clone(),
                    decision.preload_batches,
                    &transfer_session,
                )
                .await?;
                blob_preload_performed = true;
            }
            if transport == PushTransport::PhaseOneOrdinaryBootstrap {
                let target = commits.last().map(|commit| commit.hash.clone());
                publish_phase_one_ordinary_bootstrap(
                    repo,
                    work_tree,
                    remote,
                    endpoint_path,
                    name,
                    force,
                    api_key,
                    remote_head.clone(),
                    commits,
                    objects,
                    None,
                    &transfer_session,
                )
                .await?;
                if emit_result {
                    announce_transfer();
                    if quiet_stdout() {
                        output::print_line(&format!("Pushed to {remote}"));
                    } else if let Some(repo_path) = repo_path_for_web {
                        output::success(&format!(
                            "Push complete: {}",
                            super::branch_web_url(remote, repo_path, name)
                        ));
                    } else {
                        output::success("Push complete");
                    }
                }
                return Ok(finish(true, target));
            }
            if matches!(
                transport,
                PushTransport::StagedReady | PushTransport::StagedWindowReady
            ) {
                let target = repo.get_branch_head(name)?.ok_or_else(|| {
                    OakError::InvalidArgument(format!(
                        "push admission phase lost target head for branch {name}; no remote state was mutated"
                    ))
                })?;
                publish_staged_plan(
                    repo,
                    work_tree,
                    remote,
                    endpoint_path,
                    name,
                    force,
                    api_key,
                    repo_needs_creation,
                    transport == PushTransport::StagedWindowReady,
                    remote_head.clone(),
                    target.clone(),
                    commits,
                    objects,
                    None,
                    &transfer_session,
                )
                .await?;
                if emit_result {
                    announce_transfer();
                    if quiet_stdout() {
                        output::print_line(&format!("Pushed to {remote}"));
                    } else if let Some(repo_path) = repo_path_for_web {
                        output::success(&format!(
                            "Push complete: {}",
                            super::branch_web_url(remote, repo_path, name)
                        ));
                    } else {
                        output::success("Push complete");
                    }
                }
                return Ok(finish(true, Some(target)));
            }
        }
    }

    if commits.is_empty()
        && !has_preplanned_objects
        && preplanned_control
            .as_ref()
            .is_none_or(|(_, _, _, _, target, _)| target.is_none())
    {
        if let Some(name) = branch_name {
            if let Some(target) = repo.get_branch_head(name)? {
                if remote_head.as_ref() != Some(&target) {
                    if remote_head.is_some() || !repo.get_commits_for_branch(name)?.is_empty() {
                        return Err(OakError::InvalidArgument(
                            "the branch has a different published head; inspect and pull before publishing".to_string(),
                        ));
                    }
                    publish_inherited_branch_head(
                        repo,
                        &client,
                        remote,
                        endpoint_path,
                        name,
                        &target,
                        None,
                        api_key,
                    )
                    .await?;
                    return Ok(finish(true, Some(target)));
                }
            }
        }
        // Preserve empty-repo auto-creation, but keep even that persistent
        // mutation behind the same (trivially empty) admission boundary.
        admit_outgoing_wire_objects(&[], &[])?;
        if repo_needs_creation {
            create_remote_repo(&client, remote, endpoint_path, api_key).await?;
        }
        if emit_result && !bootstrapped_main {
            if quiet_stdout() {
                output::print_line("Already up to date");
            } else {
                output::info("Already up to date, nothing to push");
            }
        }
        return Ok(finish(false, remote_head));
    }

    push_info(&format!("Pushing {} commit(s)...", commits.len()));
    output::vlog(&format!(
        "push: collecting blobs/manifests for {} commit(s)",
        commits.len()
    ));
    let t_collect = std::time::Instant::now();

    // Collect blobs and tree objects
    let (mut blobs, mut trees, mut all_chunk_data) = match preplanned_objects {
        Some(objects) => (objects.blobs, objects.trees, objects.chunks),
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let mut seen_blobs = std::collections::HashSet::new();
    let mut missing_local_blobs = std::collections::HashSet::new();
    let mut seen_trees: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Track all chunk refs for large blobs so we can upload them
    let collect_from_repo = !had_preplanned_payload;

    // Progress bar for the collection phase — silent on tiny pushes (a few
    // commits finish in milliseconds and the bar would flash and disappear)
    // but essential during bootstrap batches, where walking trees + reading
    // blob bytes for hundreds of commits can take a few seconds with no
    // other visible output.
    let collect_pb = if commits.len() >= 50 {
        let pb = indicatif::ProgressBar::new(commits.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template(
                    "  Collecting [{bar:30.cyan/dim}] {pos}/{len} commits ({elapsed_precise})",
                )
                .unwrap()
                .progress_chars("━╸─"),
        );
        Some(pb)
    } else {
        None
    };

    if collect_from_repo {
        let mut tree_stack = Vec::with_capacity(commits.len());
        for commit in &commits {
            if let Some(pb) = &collect_pb {
                pb.inc(1);
            }
            tree_stack.push(commit.manifest_hash.clone());
        }
        while let Some(tree_hash) = tree_stack.pop() {
            if tree_hash == oak_core::Tree::empty_hash()
                || server_known.trees.contains(&tree_hash)
                || !seen_trees.insert(tree_hash.to_string())
            {
                continue;
            }
            let tree = repo.get_tree(&tree_hash)?.ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "push admission phase rejected local tree {tree_hash}: object is missing; no remote state was mutated"
                ))
            })?;
            trees.push(oak_core::protocol::tree_to_wire(&tree));
            for entry in tree.entries {
                match entry.kind {
                    oak_core::TreeEntryKind::Tree => tree_stack.push(entry.hash),
                    oak_core::TreeEntryKind::Blob if server_known.has_blob(&entry.hash) => {}
                    oak_core::TreeEntryKind::Blob if seen_blobs.insert(entry.hash.clone()) => {
                        // `map_outgoing_blobs` gives every still-missing
                        // blob its one chunk mapping after the server said
                        // what it lacks.
                        let admitted_by_plan = plan_admitted.contains(&entry.hash);
                        match admit_outgoing_blob(repo, &entry.hash, admitted_by_plan)? {
                            Some(blob) => blobs.push(blob),
                            None => {
                                missing_local_blobs.insert(entry.hash);
                            }
                        }
                    }
                    oak_core::TreeEntryKind::Blob => {}
                }
            }
        }
    }
    if let Some(pb) = collect_pb {
        pb.finish_and_clear();
    }

    let commit_data: Vec<CommitData> = commits.iter().map(commit_to_wire).collect();
    admit_outgoing_wire_objects(&commit_data, &trees)?;
    let staged = preplanned_control
        .as_ref()
        .is_some_and(|(_, staged, _, _, _, _)| *staged);
    let intended_published_head = if staged {
        preplanned_control
            .as_ref()
            .and_then(|(_, _, finalize, _, target, _)| finalize.then(|| target.clone()).flatten())
    } else {
        commit_data.last().map(|commit| Hash(commit.hash.clone()))
    };
    let (publication_reconciliation_commands, publication_reconciliation_note) =
        publication_reconciliation_context(repo, remote, branch_name)?;

    if !missing_local_blobs.is_empty() && repo_needs_creation {
        let mut hashes: Vec<String> = missing_local_blobs
            .iter()
            .map(ToString::to_string)
            .collect();
        hashes.sort();
        return Err(OakError::InvalidArgument(format!(
            "push admission phase is missing local blob(s) {} and the remote repository does not exist, so no remote copy can be proven; no remote state was mutated",
            hashes.join(", ")
        )));
    }
    if !missing_local_blobs.is_empty() {
        let mut hashes: Vec<String> = missing_local_blobs
            .iter()
            .map(ToString::to_string)
            .collect();
        hashes.sort();
        let negotiated = if staged {
            None
        } else {
            Some(server_push_capability(&client, remote, api_key).await?)
        };
        let require_verified_receipts = staged
            || negotiated
                .as_ref()
                .is_some_and(|capability| capability.content_receipt_enforcement_required);
        let hydrate_from_legacy = negotiated
            .as_ref()
            .is_some_and(|capability| capability.transport == ServerPushCapability::Legacy);
        // This is a metadata-only ownership/receipt query. Generic live proof
        // is rate-limited and belongs to explicit repair commands, not routine
        // push planning. Exact legacy servers still hydrate and hash the bytes
        // before mutation because they cannot negotiate receipt semantics.
        for page in hashes.chunks(BLOB_CHECK_PAGE_HASHES) {
            let proof_request = with_auth(
                client
                    .post(format!("{remote}/api/{endpoint_path}/blobs/check"))
                    .json(&BlobCheckRequest {
                        hashes: page.to_vec(),
                        require_verified_receipts,
                        verify_content: false,
                    }),
                api_key,
            );
            let proof_response = crate::http::send_idempotent_with_retry_until(
                proof_request,
                "remote missing-blob proof",
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            )
            .await
            .map_err(|error| {
                OakError::InvalidArgument(format!(
                    "push admission phase could not prove remote blob ownership for locally missing blob(s): {error}; no remote state was mutated"
                ))
            })?;
            if !proof_response.status().is_success() {
                let status = proof_response.status();
                let detail = crate::http::error_text(proof_response).await;
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase could not prove remote blob ownership for locally missing blob(s): blobs/check returned {status} ({detail}); no remote state was mutated"
                )));
            }
            let proof: BlobCheckResponse = proof_response.json().await.map_err(|error| {
                OakError::InvalidArgument(format!(
                    "push admission phase could not decode remote blob proof: {error}; no remote state was mutated"
                ))
            })?;
            validate_receipt_predicate_ack(
                &proof,
                require_verified_receipts,
                "remote missing-blob proof",
            )?;
            let mut unproven: Vec<String> =
                validate_blob_check_missing(page, proof.missing, "remote missing-blob proof")?
                    .into_iter()
                    .collect();
            unproven.sort();
            if !unproven.is_empty() {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase could not prove remote blob(s) {} that are missing locally; no remote state was mutated",
                    unproven.join(", ")
                )));
            }
            if hydrate_from_legacy {
                let (owner, name) = endpoint_path.split_once('/').ok_or_else(|| {
                    OakError::InvalidArgument(format!(
                        "invalid remote repository path {endpoint_path}; no remote state was mutated"
                    ))
                })?;
                let missing: Vec<Hash> = page
                    .iter()
                    .map(|hash| Hash::from_hex(hash))
                    .collect::<Result<Vec<_>>>()?;
                let legacy_branch = branch_name.ok_or_else(|| {
                    OakError::InvalidArgument(
                        "push admission phase cannot identify a branch for legacy blob hydration; no remote state was mutated"
                            .to_string(),
                    )
                })?;
                super::blob_fetch::ensure_blobs_local_for_legacy_push(
                    repo,
                    remote,
                    owner,
                    name,
                    legacy_branch,
                    api_key,
                    &missing,
                )
                .await
                .map_err(|error| {
                    OakError::InvalidArgument(format!(
                        "push admission phase could not restore locally missing blob bytes from the legacy server: {error}; no remote state was mutated"
                    ))
                })?;
                for hash in missing {
                    let blob = admit_outgoing_blob(repo, &hash, false)?.ok_or_else(|| {
                        OakError::InvalidArgument(format!(
                            "push admission phase restored no bytes for blob {hash}; no remote state was mutated"
                        ))
                    })?;
                    blobs.push(blob);
                }
            }
        }
    }

    // Auto-create only after the exact payload's content-addressed metadata
    // has passed admission. A damaged local commit/tree must never leave an
    // empty remote repo behind as a side effect of a rejected push.
    if repo_needs_creation && !staged {
        created_repo = create_remote_repo(&client, remote, endpoint_path, api_key).await?;
    }

    // Ask the server which of these blobs it already has so we can drop
    // their content from the push payload. Without this we re-upload every
    // blob reachable from HEAD on every push — for a 1-file commit on a
    // 250-file repo that's ~99% wasted bandwidth and an equal amount of
    // wasted R2 PUTs + Postgres no-op inserts on the server.
    //
    // Best-effort: any non-200 (e.g. older server without /blobs/check)
    // falls through to "send everything," matching the pre-dedup behavior.
    //
    // Skipped entirely when the whole inline payload is smaller than one
    // network round trip is worth: the check exists to save upload bytes,
    // and below ~64 KiB the extra sequential round trip (~100-250ms against
    // oak.space) costs more than just sending the bytes. Chunked (large)
    // blobs always go through the check — their chunk pipeline depends on
    // the server knowing what's missing.
    const BLOB_CHECK_MIN_BYTES: u64 = 64 * 1024;
    let inline_payload: u64 = blobs.iter().map(|b| b.content.len() as u64).sum();
    let skip_blob_check = !blob_preload_performed
        && !blobs.is_empty()
        // The envelope counts remote-missing blobs; never widen the request
        // past it by resending blobs the server already holds.
        && blobs.len() <= oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
        && all_chunk_data.is_empty()
        // A content-defined blob always goes through the check: its chunk
        // pipeline is only worth starting for what the server lacks.
        && !blobs.iter().any(is_deferred)
        && inline_payload < BLOB_CHECK_MIN_BYTES;
    if skip_blob_check {
        output::vlog(&format!(
            "push: skipping blobs/check ({} blob(s), {inline_payload} inline bytes < {BLOB_CHECK_MIN_BYTES})",
            blobs.len()
        ));
    }
    let mut blob_presence_confirmed = false;
    if !(blobs.is_empty()
        || skip_blob_check
        || staged && (repo_needs_creation || had_preplanned_payload))
    {
        let blob_hashes: Vec<String> = blobs.iter().map(|b| b.hash.clone()).collect();
        let t_check = std::time::Instant::now();
        let mut missing = std::collections::HashSet::new();
        let mut checked_every_page = true;
        for page in blob_hashes.chunks(BLOB_CHECK_PAGE_HASHES) {
            output::vlog(&format!(
                "push: POST {}/api/{}/blobs/check ({} hashes)",
                remote,
                endpoint_path,
                page.len()
            ));
            let request = with_auth(
                client
                    .post(format!("{remote}/api/{endpoint_path}/blobs/check"))
                    .json(&BlobCheckRequest {
                        hashes: page.to_vec(),
                        require_verified_receipts: staged,
                        verify_content: false,
                    }),
                api_key,
            );
            let resp_result = crate::http::send_idempotent_with_retry_until(
                request,
                "blob presence check",
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            )
            .await;
            match resp_result {
                Ok(resp) if resp.status().is_success() => {
                    let body: BlobCheckResponse = resp
                        .json()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    validate_receipt_predicate_ack(&body, staged, "blob presence check")?;
                    missing.extend(validate_blob_check_missing(
                        page,
                        body.missing,
                        "blob presence check",
                    )?);
                }
                Ok(resp) if resp.status().is_redirection() => {
                    return Err(crate::http::server_error(resp).await);
                }
                Ok(resp) if staged => {
                    return Err(OakError::InvalidArgument(format!(
                        "staged blob verification returned {}; no remote state was mutated",
                        resp.status()
                    )));
                }
                Ok(resp) => {
                    output::vlog(&format!(
                        "push: blobs/check returned {} in {:.3}s; sending all blobs",
                        resp.status(),
                        t_check.elapsed().as_secs_f64()
                    ));
                    checked_every_page = false;
                    break;
                }
                Err(error) if staged => {
                    return Err(OakError::InvalidArgument(format!(
                        "staged blob verification failed: {error}; no remote state was mutated"
                    )));
                }
                Err(error) => {
                    output::vlog(&format!(
                        "push: blobs/check request failed ({error}); sending all blobs"
                    ));
                    checked_every_page = false;
                    break;
                }
            }
        }
        if checked_every_page {
            blob_presence_confirmed = true;
            let before = blobs.len();
            // Always re-send the empty blob, even when the server claims
            // to have it. A server can hold a metadata-only row for it —
            // present to `blobs/check`, absent from every pull — and then
            // this dedup is precisely what stops a user from repairing
            // their repo by pushing again. It costs zero content bytes to
            // include, so trusting the check here buys nothing.
            let empty_hash = oak_core::Blob::empty_hash();
            blobs.retain(|b| missing.contains(&b.hash) || b.hash == empty_hash.as_str());
            output::vlog(&format!(
                "push: blobs/check kept {}/{} blob(s) in {:.3}s",
                blobs.len(),
                before,
                t_check.elapsed().as_secs_f64()
            ));
        }
    }

    if blob_preload_performed {
        // The publication request must be the ordinary one: it may carry
        // only what fits its envelope. Anything else means the server did
        // not confirm the blobs this push just preloaded.
        let remaining_bytes = blobs
            .iter()
            .fold(0u64, |total, blob| total.saturating_add(blob.size));
        if !blobs.is_empty()
            && (!blob_presence_confirmed
                || blobs.len() > oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
                || remaining_bytes > STAGED_CLIENT_BLOB_BATCH_BYTES)
        {
            return Err(OakError::Server(format!(
                "the server did not confirm the blobs this push preloaded ({} still reported missing); no branch head was moved and nothing was published. Preloaded blobs are content-addressed and are not re-sent; retry `oak push`",
                blobs.len()
            )));
        }
    }

    map_outgoing_blobs(&mut blobs, &mut all_chunk_data)?;

    let inline_blob_count = blobs.iter().filter(|b| !b.content.is_empty()).count() as u64;
    let total_inline_bytes: u64 = blobs.iter().map(|b| b.content.len() as u64).sum();
    output::vlog(&format!(
        "push: collected {} blob(s) ({} inline bytes), {} tree(s), {} chunk source(s) in {:.3}s",
        blobs.len(),
        total_inline_bytes,
        trees.len(),
        all_chunk_data.len(),
        t_collect.elapsed().as_secs_f64()
    ));

    // Verify and map every missing chunked blob locally before the first
    // remote mutation. Then create its exact inactive proof generation and
    // upload bounded metadata pages before any chunk upload.
    let mut uploaded_mapping_proof = None;
    let mapping_proofs = if staged {
        prepare_staged_mapping_proofs(repo, &mut blobs)?
    } else {
        Vec::new()
    };
    if staged && repo_needs_creation {
        created_repo = create_remote_repo(&client, remote, endpoint_path, api_key).await?;
    }
    if !mapping_proofs.is_empty() {
        let total_refs: usize = mapping_proofs
            .iter()
            .map(|mapping| mapping.chunks.len())
            .sum();
        let total_bytes: u64 = mapping_proofs
            .iter()
            .map(|mapping| mapping.descriptor.size)
            .sum();
        if mapping_proofs.len() > oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
            || total_refs > oak_core::protocol::MAPPING_PROOF_MAX_SET_CHUNK_REFS
            || total_bytes > oak_core::protocol::MAPPING_PROOF_MAX_SET_BYTES
        {
            return Err(OakError::InvalidArgument(
                "staged blob batch exceeds one async_v1 proof set; no remote state was mutated"
                    .to_string(),
            ));
        }
        let deadline = tokio::time::Instant::now() + transfer::size_aware_timeout(total_bytes);
        let uploaded = upload_mapping_set_with_restarts(
            &client,
            remote,
            endpoint_path,
            api_key,
            &mapping_proofs,
            deadline,
        )
        .await?;
        uploaded_mapping_proof = Some((uploaded, deadline));
    }

    // Upload the chunks the server lacks through the one uploader
    // (`crate::transfer`): bounded negotiation pages, bounded concurrency,
    // acknowledgement as uploads finish. Planned staged pushes carry only
    // content-addressed locations, so even a multi-gigabyte blob retains at
    // most one page of chunk bytes while checking/uploading.
    let chunk_declared_bytes = blobs
        .iter()
        .filter(|blob| !blob.chunks.is_empty() || is_deferred(blob))
        .fold(0u64, |total, blob| total.saturating_add(blob.size));
    let chunk_overall_deadline = uploaded_mapping_proof
        .as_ref()
        .map(|(_, deadline)| *deadline)
        .unwrap_or_else(|| {
            tokio::time::Instant::now() + transfer::size_aware_timeout(chunk_declared_bytes)
        });
    let uploader = ChunkUploader {
        remote,
        endpoint_path,
        api_key,
        retry: ChunkBusyRetryState::until(chunk_overall_deadline),
        session: transfer_session.clone(),
    };
    upload_outgoing_chunks(&uploader, repo, all_chunk_data, &mut blobs).await?;

    if let Some((uploaded, deadline)) = uploaded_mapping_proof {
        let proof_token = match finalize_mapping_set(
            &client,
            remote,
            endpoint_path,
            api_key,
            uploaded,
            deadline,
        )
        .await
        {
            Ok(token) => token,
            Err(MappingProofAttemptError::Fatal(error)) => return Err(error),
            Err(MappingProofAttemptError::Restart(mut detail)) => {
                let mut token = None;
                for _ in 1..MAPPING_PROOF_MAX_RESTARTS {
                    match prove_mapping_set_once(
                        &client,
                        remote,
                        endpoint_path,
                        api_key,
                        &mapping_proofs,
                        deadline,
                    )
                    .await
                    {
                        Ok(value) => {
                            token = Some(value);
                            break;
                        }
                        Err(MappingProofAttemptError::Restart(next)) => detail = next,
                        Err(MappingProofAttemptError::Fatal(error)) => return Err(error),
                    }
                }
                token.ok_or_else(|| {
                    OakError::InvalidArgument(format!(
                        "async_v1 mapping finalization could not restart safely after {MAPPING_PROOF_MAX_RESTARTS} attempts ({detail}); no head was advanced"
                    ))
                })?
            }
        };
        for blob in &mut blobs {
            if !blob.chunks.is_empty() {
                blob.mapping_proof_token = Some(proof_token.clone());
            }
        }
    }

    // Build branch data if we have a current branch
    let description_generation = repo.description_publication_generation()?;
    let branch_data = if let Some(name) = branch_name {
        repo.get_branch(name)?.as_ref().map(branch_to_push_data)
    } else {
        None
    };
    let published_description = branch_data
        .as_ref()
        .map(|branch| (branch.name.clone(), branch.description.clone()));
    let description_is_published = !staged
        || preplanned_control
            .as_ref()
            .is_some_and(|(_, _, finalize, _, _, _)| *finalize);

    // Staged publication uses a structurally versioned endpoint. Never send
    // staging semantics to legacy `/push`: an old mixed-deployment replica
    // could ignore unknown fields and expose an intermediate side-line head.
    let request_path = if staged && window_budget {
        format!("{remote}/api/{endpoint_path}/push/staged-window-v1")
    } else if staged {
        format!("{remote}/api/{endpoint_path}/push/staged-v1")
    } else {
        format!("{remote}/api/{endpoint_path}/push")
    };
    let commit_count = commit_data.len();
    let blob_count = blobs.len();
    let tree_count = trees.len();
    output::vlog(&format!(
        "push: POST {request_path} ({commit_count} commit(s), {blob_count} blob(s), {tree_count} tree(s))"
    ));
    let t_push = std::time::Instant::now();
    let mut publication_attempt;
    let push_resp: PushResponse = if staged {
        let (stage_id, _, finalize, expected_head, target_head, final_handoff) =
            preplanned_control.expect("staged request has preplanned control");
        let branch = branch_data.ok_or_else(|| {
            OakError::InvalidArgument(
                "staged push requires explicit branch metadata; no remote state was mutated"
                    .to_string(),
            )
        })?;
        let attempt_expected = expected_head.clone();
        let attempt_target = target_head.clone();
        let staged_request = StagedPushRequest {
            stage_id: stage_id.ok_or_else(|| {
                OakError::InvalidArgument(
                    "staged push is missing its operation session id; no remote state was mutated"
                        .to_string(),
                )
            })?,
            expected_branch_head: expected_head.map(|hash| hash.to_string()),
            branch,
            finalize,
            force,
            target_head: target_head.map(|hash| hash.to_string()),
            commits: commit_data,
            blobs,
            trees,
        };
        let body = serde_json::to_vec(&staged_request).map_err(|error| {
            OakError::InvalidArgument(format!(
                "could not encode staged publication request: {error}; no remote state was mutated"
            ))
        })?;
        let request = with_auth(
            client
                .post(&request_path)
                .header("x-oak-user", super::commit::get_author())
                .header(reqwest::header::CONTENT_TYPE, "application/json"),
            api_key,
        );
        publication_attempt = prepare_publication_attempt(
            work_tree,
            remote,
            endpoint_path,
            PublicationAttemptInput {
                branch: Some(&staged_request.branch),
                target: attempt_target.as_ref(),
                expected: attempt_expected.as_ref(),
                force,
                transport: crate::publication_state::PublicationTransport::StagedFinal,
                body: &body,
                request_url: &request_path,
                final_handoff: final_handoff.as_ref(),
            },
        )?;
        match send_staged_publication_with_cap(
            request,
            body,
            STAGED_PUBLICATION_REQUEST_TIMEOUT,
            &publication_reconciliation_commands,
            publication_reconciliation_note,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let error = publication_error_with_operation(error, publication_attempt.as_ref());
                if !matches!(error, OakError::PublicationUnconfirmed { .. }) {
                    reject_and_retire_publication_attempt(&mut publication_attempt);
                }
                return Err(error);
            }
        }
    } else {
        let ordinary_request = PushRequest {
            expected_head: if force {
                None
            } else {
                remote_head.as_ref().map(|hash| hash.to_string())
            },
            expected_branch_head: None,
            force,
            branch: branch_data,
            commits: commit_data,
            blobs,
            trees,
        };
        let body = serde_json::to_vec(&ordinary_request).map_err(|_| {
            OakError::InvalidArgument(
                "could not encode ordinary publication request; no remote state was mutated"
                    .to_string(),
            )
        })?;
        let request = with_auth(
            client
                .post(&request_path)
                .header("x-oak-user", super::commit::get_author())
                .header(reqwest::header::CONTENT_TYPE, "application/json"),
            api_key,
        );
        publication_attempt = prepare_publication_attempt(
            work_tree,
            remote,
            endpoint_path,
            PublicationAttemptInput {
                branch: ordinary_request.branch.as_ref(),
                target: intended_published_head.as_ref(),
                expected: remote_head.as_ref(),
                force,
                transport: crate::publication_state::PublicationTransport::Ordinary,
                body: &body,
                request_url: &request_path,
                final_handoff: None,
            },
        )?;
        let response = match crate::http::send_publication_request(
            request,
            body,
            "push publication",
            &publication_reconciliation_commands,
        )
        .await
        .map_err(|error| annotate_publication_error(error, publication_reconciliation_note))
        {
            Ok(response) => response,
            Err(error) => {
                return Err(publication_error_with_operation(
                    error,
                    publication_attempt.as_ref(),
                ));
            }
        };
        output::vlog(&format!(
            "push: publication returned {} in {:.3}s",
            response.status(),
            t_push.elapsed().as_secs_f64()
        ));
        if !response.status().is_success() {
            let status = response.status();
            if crate::http::publication_status_is_unconfirmed(status) {
                return Err(publication_error_with_operation(
                    crate::http::publication_unconfirmed(
                        "push publication",
                        publication_reason(
                            format!("the server returned HTTP {status}"),
                            publication_reconciliation_note,
                        ),
                        publication_reconciliation_commands,
                    ),
                    publication_attempt.as_ref(),
                ));
            }
            reject_and_retire_publication_attempt(&mut publication_attempt);
            return Err(OakError::Server(format!(
                "Push publication was rejected with HTTP {status}; the response body was omitted because remote diagnostics may contain secrets"
            )));
        }
        let body = crate::http::bounded_publication_body(response)
            .await
            .map_err(|reason| {
                publication_error_with_operation(
                    crate::http::publication_unconfirmed(
                        "push publication",
                        publication_reason(reason.to_string(), publication_reconciliation_note),
                        publication_reconciliation_commands.clone(),
                    ),
                    publication_attempt.as_ref(),
                )
            })?;
        serde_json::from_slice(&body).map_err(|_| {
            publication_error_with_operation(
                crate::http::publication_unconfirmed(
                    "push publication",
                    publication_reason(
                        "the successful response did not contain a valid receipt",
                        publication_reconciliation_note,
                    ),
                    publication_reconciliation_commands.clone(),
                ),
                publication_attempt.as_ref(),
            )
        })?
    };
    output::vlog(&format!(
        "push: publication completed in {:.3}s",
        t_push.elapsed().as_secs_f64()
    ));

    if let Err(error) = validate_successful_publication_ack(
        &push_resp,
        intended_published_head.as_ref(),
        "push publication",
        &publication_reconciliation_commands,
    )
    .map_err(|error| annotate_publication_error(error, publication_reconciliation_note))
    {
        return Err(publication_error_with_operation(
            error,
            publication_attempt.as_ref(),
        ));
    }

    if push_resp.success {
        // Inline blobs are their own single chunks, carried by the request
        // the server just acknowledged.
        transfer_session.record_inline_blobs(inline_blob_count, total_inline_bytes);
        acknowledge_and_finish_publication_attempt(
            &mut publication_attempt,
            work_tree,
            remote,
            endpoint_path,
            branch_name,
            intended_published_head.as_ref(),
        )?;
        if description_is_published {
            if let Some((name, description)) = &published_description {
                acknowledge_published_description(
                    repo,
                    name,
                    description.as_deref(),
                    description_generation,
                );
            }
        }
        if !emit_result {
            return Ok(finish(
                intended_published_head.is_some(),
                intended_published_head,
            ));
        }
        announce_transfer();
        if quiet_stdout() {
            output::print_line(&format!("Pushed to {remote}"));
        } else if let (Some(repo_path), Some(branch)) = (repo_path_for_web, branch_name) {
            let url = super::branch_web_url(remote, repo_path, branch);
            output::success(&format!("Push complete: {url}"));
        } else {
            output::success("Push complete");
        }
    } else {
        reject_and_retire_publication_attempt(&mut publication_attempt);
        return Err(OakError::ConflictDetected);
    }

    Ok(EndpointPushOutcome {
        repo_created: created_repo,
        ..finish(intended_published_head.is_some(), intended_published_head)
    })
}

/// First push to a freshly-created repo: the push lands on the user's
/// personal/feature branch, but `main` stays empty. The web UI's Code tab and
/// any Pages site then look empty — which reads like the push silently
/// failed. Offer to land the work on main right here so the user doesn't have
/// to discover the squash-merge step on their own.
///
/// Call only after the checkout's remote, owner and repo name are saved: the
/// merge reads them from local metadata, and a first push saves them only
/// once it succeeds.
async fn offer_land_on_main_after_first_push(
    repo: &SqliteRepository,
    work_tree: &Path,
    name: &str,
) -> Result<()> {
    let parented_on_main = repo
        .get_branch(name)
        .ok()
        .flatten()
        .and_then(|b| b.parent_branch)
        .is_some_and(|p| p == "main");
    if !parented_on_main {
        return Ok(());
    }
    // The merge resets the working tree, so it needs the workdir lock. If
    // another oak process already owns it — typically an `oak merge` that
    // initiated this very push — that process is about to merge itself, so
    // skip the offer entirely.
    let lock = crate::resolve::resolve(work_tree)
        .ok()
        .and_then(|ctx| crate::workdir_lock::WorkdirLock::try_acquire(&ctx.oak_dir).ok());
    if let Some(lock) = lock.filter(|_| is_interactive()) {
        let prompt = format!(
            "Land '{name}' on main now? Otherwise the repo will look empty in the web UI until you run `oak merge`."
        );
        let confirm = Confirm::new()
            .with_prompt(prompt)
            .default(true)
            .interact()
            .unwrap_or(false);
        if confirm {
            super::merge::merge_to_main_via_server(&lock, repo, work_tree, name, false, false)
                .await?;
        }
    } else if !is_interactive() {
        output::info(
            "Run `oak merge` to land this on main — otherwise the repo will look empty in the web UI.",
        );
    }
    Ok(())
}

/// Whether stdin is a TTY — used to skip interactive prompts in scripted
/// pushes so they don't hang waiting for input.
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Invariant 3: the server's branch head isn't in local history. Fetch it;
/// when it's just a moved seed (a `main` commit — the remote branch has no
/// real work of its own), re-parent the branch onto it and let the push
/// continue in this same invocation, printing exactly one info line. When
/// the remote branch holds real foreign commits — or anything about the
/// self-heal can't be established — fail with `oak pull` as the single
/// instruction (pull now converges instead of dead-ending).
async fn self_heal_diverged_push(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    api_key: Option<&str>,
) -> Result<()> {
    let Some(branch) = branch_name.filter(|b| *b != "main") else {
        return Err(OakError::RemoteCommitsNotInLocalHistory);
    };
    let Some((owner, repo_name)) = endpoint_path.split_once('/') else {
        return Err(OakError::RemoteCommitsNotInLocalHistory);
    };

    // Probe dirtiness BEFORE any pointer moves: the probe compares the
    // working tree against the current head.
    let worktree_clean =
        super::commit::worktree_is_clean_without_storing_blobs(repo, work_tree).unwrap_or(false);
    if !worktree_clean {
        // A re-parent changes the base of every working-tree file. Carrying
        // dirty bytes unchanged can make the next commit revert main's work.
        return Err(OakError::RemoteCommitsNotInLocalHistory);
    }

    let check = match super::sync::prepare_reparent(repo, remote, owner, repo_name, branch, api_key)
        .await
    {
        Ok(check) => check,
        Err(e) => {
            // Older server without /commits/info, network blip, …: we
            // can't establish that the heal is safe. Keep the actionable
            // error; `oak pull` converges.
            output::vlog(&format!("push: re-parent self-heal unavailable ({e})"));
            return Err(OakError::RemoteCommitsNotInLocalHistory);
        }
    };

    match check {
        // Raced: the head landed locally in the meantime — push proceeds.
        super::sync::ReparentCheck::AlreadyAnchored => Ok(()),
        // The remote is simply ahead of this checkout: nothing to heal on
        // push; `oak pull` fast-forwards.
        super::sync::ReparentCheck::RemoteContainsTip { .. } => {
            Err(OakError::RemoteCommitsNotInLocalHistory)
        }
        super::sync::ReparentCheck::Ready(plan) => {
            // Real foreign commits on the remote branch, or overlapping
            // edits: not trivially safe — one instruction, `oak pull`.
            if plan.seed_branch_name == branch || plan.conflict_count() > 0 {
                return Err(OakError::RemoteCommitsNotInLocalHistory);
            }
            // The overlay can change worktree content (paths main changed),
            // so a CLEAN tree must be reset to it — which needs the workdir
            // lock. When someone else holds it (e.g. the commit or merge
            // flow that initiated this push), bail with the actionable
            // error rather than leave a clean tree silently out of sync
            // with its new head; the next standalone `oak push` or
            // `oak pull` heals fully. Dirty trees were deferred above.
            let lock = crate::resolve::resolve(work_tree)
                .ok()
                .and_then(|ctx| crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir).ok());
            if lock.is_none()
                || !super::commit::worktree_is_clean_without_storing_blobs(repo, work_tree)
                    .unwrap_or(false)
            {
                // Preparation awaits network I/O. Recheck while holding the
                // lock so work saved during that wait is never reset away.
                output::vlog("push: self-heal needs a locked, still-clean worktree; deferring");
                return Err(OakError::RemoteCommitsNotInLocalHistory);
            }
            super::sync::complete_reparent(
                lock.as_ref(),
                repo,
                work_tree,
                branch,
                &plan,
                worktree_clean,
            )?;
            let line = format!(
                "re-parented onto {}@{} (main advanced since this branch was created)",
                plan.seed_branch_name,
                plan.seed.short()
            );
            if quiet_stdout() {
                output::print_line(&line);
            } else {
                output::info(&line);
            }
            Ok(())
        }
    }
}

/// GET `/api/{endpoint}/branches/{branch}` and pull out the head hash.
/// Returns `None` on 404 (branch doesn't exist on the server yet).
/// Used by the bootstrap loop to drive iteration progress without
/// duplicating the per-branch GET that `push_async_with_endpoint`
/// already does internally, and by `oak switch` to resolve the head of a
/// remote branch that has no commits of its own.
/// A stacked branch carries its parent's commits, which the server can only
/// admit once the parent itself is published. Refuse up front with the next
/// step instead of a history-proof error from the server.
async fn require_stacked_parent_published(
    client: &reqwest::Client,
    repo: &SqliteRepository,
    remote: &str,
    endpoint_path: &str,
    branch_name: &str,
    repo_needs_creation: bool,
    api_key: Option<&str>,
) -> Result<()> {
    let Some(parent) = repo
        .get_branch(branch_name)?
        .and_then(|branch| branch.parent_branch)
        .filter(|parent| parent != oak_core::DEFAULT_BRANCH && parent != branch_name)
    else {
        return Ok(());
    };
    let published = !repo_needs_creation
        && fetch_remote_branch_head(client, remote, endpoint_path, &parent, api_key)
            .await?
            .is_some();
    if published {
        return Ok(());
    }
    Err(OakError::InvalidArgument(format!(
        "'{branch_name}' is stacked on '{parent}', which isn't on the remote yet. \
         Push '{parent}' first (`oak switch {parent} && oak push`), then push '{branch_name}'."
    )))
}

pub(crate) async fn fetch_remote_branch_head(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    branch: &str,
    api_key: Option<&str>,
) -> Result<Option<Hash>> {
    let resp = with_auth(
        client.get(format!(
            "{remote}/api/{endpoint_path}/branches/{}",
            super::branch_api_segment(branch)
        )),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().is_success() {
        let body: BranchHeadResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        Ok(body.head.map(Hash))
    } else if resp.status().as_u16() == 404 {
        Ok(None)
    } else {
        Err(crate::http::server_error(resp).await)
    }
}

#[derive(Deserialize)]
struct OrganizationItem {
    slug: String,
    name: String,
    emoji: Option<String>,
    role: Option<String>,
}

#[derive(Deserialize)]
struct OrganizationListResp {
    organizations: Vec<OrganizationItem>,
}

/// Non-interactive first-push linking from `--repo ORG/REPO` (or `OAK_REPO`).
/// Parses and validates the provisional spec without changing local linkage. Lets
/// the downstream push auto-create the repo on the server when it 404s. ORG
/// must already be an organization slug you can push to.
fn parse_remote_identity(spec: &str) -> Result<(String, String)> {
    let (owner, name) = super::parse_owner_repo(spec).map_err(|err| {
        OakError::InvalidArgument(format!(
            "Invalid --repo value '{spec}'. Use ORG/REPO. {err}"
        ))
    })?;

    Ok((owner, name))
}

/// Interactive first-push setup: pick or create an organization, choose a repo
/// name, returning a provisional identity without changing local linkage. The downstream
/// push flow auto-creates the repo on the server when it 404s.
async fn setup_remote_identity(
    repo: &SqliteRepository,
    remote: &str,
    api_key: Option<&str>,
) -> Result<(String, String)> {
    // No TTY → the org picker below would hang. Fail with the exact flag to
    // re-run with, so scripted / agent pushes get an actionable error instead
    // of a stuck prompt.
    if !is_interactive() {
        return Err(OakError::RepoNotLinked(format!(
            "This repository isn't linked to a remote, and there's no terminal for the \
             interactive org picker. Re-run with `oak push --repo <org>/<repo>` (or set \
             OAK_REPO=<org>/<repo>), where <org> is an existing organization slug on \
             {remote}. Create the org first on {remote} if you don't have one yet."
        )));
    }

    output::blank();
    output::header("This repository isn't linked to a remote yet");
    output::info(&format!(
        "Pick an organization on {remote} to push to. The repository will be created there."
    ));
    output::blank();

    let proceed = Confirm::new()
        .with_prompt("Configure remote organization and repo name?")
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    if !proceed {
        return Err(OakError::Server(
            "Push cancelled. Run `oak push` again to configure the remote, or `oak clone` to pick an existing repo.".to_string(),
        ));
    }

    // `api_key` was already resolved for this remote (process override, then
    // the origin-bound repository key, then the stored login).
    let token = api_key
        .map(String::from)
        .or_else(|| super::credentials::effective_token(remote));
    let token = match token {
        Some(t) => t,
        None => {
            output::blank();
            output::info(&format!("You're not logged in to {remote}."));
            let login_now = Confirm::new()
                .with_prompt("Log in now?")
                .default(true)
                .interact()
                .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
            if !login_now {
                return Err(OakError::Server(format!(
                    "Authentication required to push a new repo. Run `oak login -r {remote}` and try again."
                )));
            }
            super::login::login_and_save(remote).await?;
            super::credentials::effective_token(remote).ok_or_else(|| {
                OakError::Server(
                    "Login completed but no credential was saved. Try `oak login` again."
                        .to_string(),
                )
            })?
        }
    };

    let client = crate::http::api_client();
    let resp = client
        .get(format!("{remote}/api/orgs"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Failed to fetch organizations: {}",
            crate::http::error_text(resp).await
        )));
    }
    let list: OrganizationListResp = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let owner = if list.organizations.is_empty() {
        output::info("You don't belong to any organizations yet. Let's create one.");
        create_organization_interactive(&client, remote, &token).await?
    } else {
        let mut items: Vec<String> = list
            .organizations
            .iter()
            .map(|ws| {
                let emoji = ws
                    .emoji
                    .as_deref()
                    .map(|e| format!("{e} "))
                    .unwrap_or_default();
                let role = ws
                    .role
                    .as_deref()
                    .map(|r| format!(" [{r}]"))
                    .unwrap_or_default();
                format!("{}{} ({}){}", emoji, ws.name, ws.slug, role)
            })
            .collect();
        items.push("+ Create new organization…".to_string());

        let idx = Select::new()
            .with_prompt("Organization")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

        if idx == list.organizations.len() {
            create_organization_interactive(&client, remote, &token).await?
        } else {
            list.organizations[idx].slug.clone()
        }
    };

    let default_name = repo
        .get_metadata(MetadataKey::RepoName)?
        .unwrap_or_default();
    let name: String = Input::new()
        .with_prompt(format!("Repo name (under {owner})"))
        .with_initial_text(&default_name)
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    let name = name.trim().to_string();
    if name.is_empty() || name.contains('/') || name.contains(' ') || name.contains('\\') {
        return Err(OakError::Server(
            "Repo name cannot be empty or contain spaces, '/', or '\\'".to_string(),
        ));
    }

    Ok((owner, name))
}

async fn create_organization_interactive(
    client: &reqwest::Client,
    remote: &str,
    token: &str,
) -> Result<String> {
    let slug: String = Input::new()
        .with_prompt("New organization slug (lowercase, no spaces)")
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    let slug = slug.trim().to_string();
    if slug.is_empty() || slug.contains('/') || slug.contains(' ') {
        return Err(OakError::Server(
            "Organization slug cannot be empty or contain spaces or '/'".to_string(),
        ));
    }

    let display_name: String = Input::new()
        .with_prompt("Organization display name")
        .with_initial_text(&slug)
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    let resp = client
        .post(format!("{remote}/api/orgs"))
        .header("authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "slug": slug,
            "name": display_name.trim(),
        }))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Failed to create organization: {}",
            crate::http::error_text(resp).await
        )));
    }

    output::success(&format!("Created organization '{slug}'"));
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::{
        admit_local_blob_descriptor, admit_outgoing_blob, best_effort_abort_staged_session,
        best_effort_abort_staged_session_with_timeout, collect_planned_objects, commit_to_wire,
        local_blob_mapping, map_outgoing_blobs, materialize_planned_blob_batch,
        parse_remote_identity, plan_outgoing_commits, prepare_staged_mapping_proofs,
        prove_mapping_set_once, publication_reconciliation_context, remote_missing_staged_blobs,
        select_staged_protocol, send_mapping_proof_request_with_cap,
        send_staged_publication_with_cap, server_push_capability, split_staged_blob_batches,
        split_staged_tree_batches_with_limits, staged_protocol_required,
        staged_session_capability_available, upload_mapping_set_once,
        upload_mapping_set_with_restarts, validate_blob_check_missing,
        validate_external_edge_proofs, validate_push_operation_caps,
        validate_receipt_predicate_ack, validate_successful_publication_ack, PlannedBlob,
        PreparedChunk, PreparedMappingProof, PushOperationTotals, ServerPushCapability,
        BOOTSTRAP_BATCH_SIZE, STAGED_PUBLICATION_RESPONSE_MAX_BYTES,
    };
    use oak_core::OakError;
    use oak_core::{
        hash_bytes,
        protocol::{tree_to_wire, BlobCheckResponse, BlobData, PushResponse},
        Blob, Branch, ChunkInfo, Commit, FileMode, Hash, ManifestEntry, MetadataKey, Repository,
        SqliteRepository, Tree, TreeEntry, TreeEntryKind,
    };

    fn temp_repo() -> (tempfile::TempDir, SqliteRepository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn inherited_head_requires_exact_capability_and_exact_open_receipt() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let (_dir, repo) = temp_repo();
        repo.store_branch(&Branch::new(
            "task".into(),
            Some("work".into()),
            Some("main".into()),
        ))
        .unwrap();
        let head = hash_bytes(b"published parent");
        let valid = serde_json::json!({"name":"task","head":head.as_str(),"description":"work","parent_branch":"main","status":"open"});
        for capability in [
            serde_json::json!({}),
            serde_json::json!({"branch_seed_protocol":"parent_head_v1"}),
            serde_json::json!({"branch_seed_protocol":"future_v2"}),
            serde_json::json!({"branch_seed_protocol":true}),
            serde_json::json!([]),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/api/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(capability))
                .mount(&server)
                .await;
            let result = super::publish_inherited_branch_head(
                &repo,
                &crate::http::api_client(),
                &server.uri(),
                "acme/demo",
                "task",
                &head,
                None,
                None,
            )
            .await;
            assert!(matches!(result, Err(OakError::InvalidArgument(_))));
            assert!(server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == "GET"));
            assert_eq!(repo.get_branch_head("task").unwrap(), None);
        }
        for key in ["name", "head", "description", "parent_branch", "status"] {
            let server = MockServer::start().await;
            Mock::given(path("/api/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"branch_seed_protocol":"parent_head_cas_v1"}),
                ))
                .mount(&server)
                .await;
            let mut receipt = valid.clone();
            receipt[key] = "wrong".into();
            Mock::given(method("POST"))
                .and(path("/api/acme/demo/branches"))
                .respond_with(ResponseTemplate::new(200).set_body_json(receipt))
                .expect(1)
                .mount(&server)
                .await;
            let result = super::publish_inherited_branch_head(
                &repo,
                &crate::http::api_client(),
                &server.uri(),
                "acme/demo",
                "task",
                &head,
                None,
                None,
            )
            .await;
            match result {
                Err(OakError::PublicationUnconfirmed {
                    reconciliation_commands,
                    ..
                }) => {
                    assert!(!reconciliation_commands.is_empty());
                    assert!(reconciliation_commands
                        .iter()
                        .all(|s| !s.starts_with("oak push")));
                }
                other => panic!("expected unconfirmed: {other:?}"),
            }
            assert_eq!(repo.get_branch_head("task").unwrap(), None);
        }
    }

    #[tokio::test]
    async fn inherited_head_lost_success_is_unconfirmed_and_not_retried() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };
        let (_dir, repo) = temp_repo();
        repo.store_branch(&Branch::new(
            "task".into(),
            Some("work".into()),
            Some("main".into()),
        ))
        .unwrap();
        let head = hash_bytes(b"published parent");
        for response in [
            ResponseTemplate::new(503),
            ResponseTemplate::new(200).set_body_string("truncated{"),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/api/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"branch_seed_protocol":"parent_head_cas_v1"}),
                ))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/acme/demo/branches"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let result = super::publish_inherited_branch_head(
                &repo,
                &crate::http::api_client(),
                &server.uri(),
                "acme/demo",
                "task",
                &head,
                None,
                None,
            )
            .await;
            assert!(matches!(
                result,
                Err(OakError::PublicationUnconfirmed { .. })
            ));
            assert_eq!(repo.get_branch_head("task").unwrap(), None);
            assert_eq!(
                server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|r| r.method == "POST")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn push_admission_synthesizes_the_canonical_empty_blob() {
        let (_dir, repo) = temp_repo();
        let empty = Blob::empty_hash();

        assert!(!repo.has_blob(&empty).unwrap());
        let descriptor = admit_local_blob_descriptor(&repo, &empty)
            .unwrap()
            .expect("canonical empty blob must be derivable during admission");

        assert_eq!(descriptor.hash, empty);
        assert_eq!(descriptor.size, 0);
        assert_eq!(descriptor.chunk_refs, 0);
        assert!(repo.has_blob(&empty).unwrap());
    }

    #[test]
    fn external_edge_proof_requires_the_exact_requested_set() {
        let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let requested = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp,
        )
        .unwrap();
        let extra = Commit::with_timestamp(
            "main".to_string(),
            Some(requested.hash.clone()),
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(1),
        )
        .unwrap();
        let expected = std::collections::HashSet::from([requested.hash.clone()]);

        let unknown = validate_external_edge_proofs(
            &expected,
            vec![commit_to_wire(&requested), commit_to_wire(&extra)],
        )
        .unwrap_err();
        assert!(unknown
            .to_string()
            .contains("unrequested remote commit proof"));
        assert!(unknown.to_string().contains(extra.hash.as_str()));

        let duplicate = validate_external_edge_proofs(
            &expected,
            vec![commit_to_wire(&requested), commit_to_wire(&requested)],
        )
        .unwrap_err();
        assert!(duplicate
            .to_string()
            .contains("duplicate remote commit proof"));
        assert!(duplicate.to_string().contains(requested.hash.as_str()));

        let missing = validate_external_edge_proofs(&expected, Vec::new()).unwrap_err();
        assert!(missing
            .to_string()
            .contains("could not prove older remote commit edge"));
        assert!(missing.to_string().contains(requested.hash.as_str()));

        validate_external_edge_proofs(&expected, vec![commit_to_wire(&requested)]).unwrap();
    }

    #[test]
    fn blob_check_missing_must_be_a_unique_subset_of_the_request() {
        let requested = vec!["aa".repeat(32), "bb".repeat(32)];
        let unknown =
            validate_blob_check_missing(&requested, vec!["cc".repeat(32)], "blob presence check")
                .unwrap_err();
        assert!(unknown.to_string().contains("unrequested hash"));

        let duplicate = validate_blob_check_missing(
            &requested,
            vec![requested[0].clone(), requested[0].clone()],
            "blob presence check",
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("duplicate hash"));

        let missing = validate_blob_check_missing(
            &requested,
            vec![requested[1].clone()],
            "blob presence check",
        )
        .unwrap();
        assert_eq!(
            missing,
            std::collections::HashSet::from([requested[1].clone()])
        );
    }

    #[test]
    fn strict_receipt_check_requires_an_exact_server_acknowledgement() {
        let legacy_or_old_replica = BlobCheckResponse {
            missing: Vec::new(),
            verified_content: false,
            verified_receipts_required: false,
        };
        let error =
            validate_receipt_predicate_ack(&legacy_or_old_replica, true, "staged blob check")
                .unwrap_err();
        assert!(error.to_string().contains("did not acknowledge"));
        assert!(error.to_string().contains("No remote state was mutated"));

        let strict = BlobCheckResponse {
            missing: Vec::new(),
            verified_content: false,
            verified_receipts_required: true,
        };
        validate_receipt_predicate_ack(&strict, true, "staged blob check").unwrap();
        validate_receipt_predicate_ack(&legacy_or_old_replica, false, "ordinary blob check")
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn strict_receipt_check_rejects_a_mixed_old_replica_before_mutation() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let hash = Hash::from_hex(&"ab".repeat(32)).unwrap();
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/check"))
            .and(body_json(serde_json::json!({
                "hashes": [hash.as_str()],
                "require_verified_receipts": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = remote_missing_staged_blobs(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            None,
            &[PlannedBlob {
                hash,
                size: 1,
                chunk_refs: 1,
            }],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("did not acknowledge"));
        assert!(error.to_string().contains("No remote state was mutated"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase_one_bootstrap_requires_its_explicit_capability() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn capability(extra: serde_json::Value) -> ServerPushCapability {
            let server = MockServer::start().await;
            let mut value = serde_json::json!({
                "push_protocol": "legacy",
                "staged_session_protocol": "opaque_v1",
                "staged_capabilities_ready": false,
                "staged_abort_protocol": "v1",
                "known_loss_protocol": "report_v1"
            });
            if let Some(extra) = extra.as_object() {
                value.as_object_mut().unwrap().extend(extra.clone());
            }
            Mock::given(method("GET"))
                .and(path("/api/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(value))
                .expect(1)
                .mount(&server)
                .await;
            server_push_capability(&crate::http::api_client(), &server.uri(), None)
                .await
                .unwrap()
                .transport
        }

        assert_eq!(
            capability(serde_json::json!({
                "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL
            }))
            .await,
            ServerPushCapability::Legacy,
            "an unrelated transport capability must not authorize headless preload"
        );
        assert_eq!(
            capability(serde_json::json!({
                "ordinary_bootstrap_protocol": oak_core::protocol::ORDINARY_BOOTSTRAP_PROTOCOL
            }))
            .await,
            ServerPushCapability::PhaseOneOrdinaryBootstrap
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_abort_posts_exact_session_boundary_with_auth() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/push/staged-v1/0123456789abcdef/abort"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(serde_json::json!({
                "branch_name": "main",
                "expected_branch_head": null
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "aborted": true,
                "state": "aborted"
            })))
            .expect(1)
            .mount(&server)
            .await;

        best_effort_abort_staged_session(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            "0123456789abcdef",
            "main",
            None,
            Some("secret"),
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_abort_never_waits_for_a_peer_that_withholds_headers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/push/staged-v1/stuck/abort"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        best_effort_abort_staged_session_with_timeout(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            "stuck",
            "main",
            None,
            None,
            std::time::Duration::from_millis(25),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_capability_probe_falls_back_when_exact_abort_v1_is_absent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "push_protocol": "staged_v1",
                "staged_session_protocol": "opaque_v1",
                "staged_capabilities_ready": true,
                "mapping_proof_protocol": "async_v1",
                "staged_abort_protocol": "v2"
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert!(!staged_session_capability_available(
            &crate::http::api_client(),
            &server.uri(),
            None,
        )
        .await
        .unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_publication_does_not_replay_when_final_response_body_is_withheld() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                received.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&received[..header_end]);
                    let content_length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (header_end, content_length);
                }
            };
            while received.len() < header_end + content_length {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                received.extend_from_slice(&buffer[..read]);
            }
            received[header_end..header_end + content_length].to_vec()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (bodies_tx, mut bodies_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            bodies_tx.send(read_request(&mut stream).await).unwrap();
            let body = br#"{"success":true,"new_head":null,"message":"ok"}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(b"{").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let request_body = br#"{"stage_id":"same-request"}"#.to_vec();
        let result = send_staged_publication_with_cap(
            crate::http::api_client()
                .post(format!("http://{address}/api/oak/oak/push/staged-v1"))
                .header(reqwest::header::CONTENT_TYPE, "application/json"),
            request_body.clone(),
            std::time::Duration::from_millis(50),
            &[],
            None,
        )
        .await;
        assert!(matches!(
            result,
            Err(OakError::PublicationUnconfirmed { .. })
        ));
        assert_eq!(bodies_rx.recv().await.unwrap(), request_body);
        assert!(bodies_rx.recv().await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_final_ambiguity_persists_one_restart_record() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().unwrap();
        crate::commands::init::run(dir.path(), false).unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "staged target\n").unwrap();
        crate::commands::commit::run(dir.path()).unwrap();
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        let branch = repo.get_current_branch_name().unwrap().unwrap();
        let target = repo.get_branch_head(&branch).unwrap().unwrap();
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/oak/oak/branches/{branch}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/push/staged-v1"))
            .respond_with(ResponseTemplate::new(502))
            .expect(1)
            .mount(&server)
            .await;

        let error = super::push_async_with_endpoint_outcome(
            &repo,
            dir.path(),
            &server.uri(),
            "oak/oak",
            Some(&branch),
            false,
            Some("test-token"),
            Some(super::PreplannedBatch {
                window_budget: false,
                stage_id: Some("staged-final-test".to_string()),
                expected_head: None,
                commits: Vec::new(),
                staged_protocol: true,
                finalize: true,
                target_head: Some(target),
                objects: Some(super::PreplannedObjects {
                    trees: Vec::new(),
                    blobs: Vec::new(),
                    chunks: Vec::new(),
                }),
                final_handoff: None,
                transfer: crate::transfer::TransferSession::new(),
            }),
            Some("oak/oak"),
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, OakError::PublicationUnconfirmed { .. }));
        let page = crate::publication_state::AttemptStore::checkout(&dir.path().join(".oak"))
            .page(None, 50)
            .unwrap();
        assert_eq!(page.pending_publication_count, 1);
        assert_eq!(
            page.pending_publications[0].transport,
            crate::publication_state::PublicationTransport::StagedFinal
        );
        assert_eq!(
            page.pending_publications[0].state,
            crate::publication_state::AttemptState::SentUnconfirmed
        );
    }

    #[test]
    fn durable_request_identity_binds_the_actual_publication_route() {
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().unwrap();
        crate::commands::init::run(dir.path(), false).unwrap();
        let target = Hash("a".repeat(64));
        let branch = super::BranchData {
            name: "topic".to_string(),
            description: None,
            parent_branch: Some("main".to_string()),
            status: "open".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            close_reason: None,
        };
        let body = b"exact staged body";
        let handle = super::prepare_publication_attempt(
            dir.path(),
            "https://example.com/base",
            "owner/repo",
            super::PublicationAttemptInput {
                branch: Some(&branch),
                target: Some(&target),
                expected: None,
                force: false,
                transport: crate::publication_state::PublicationTransport::StagedFinal,
                body,
                request_url: "https://example.com/base/api/owner/repo/push/staged-v1",
                final_handoff: None,
            },
        )
        .unwrap()
        .unwrap();
        let page = crate::publication_state::AttemptStore::checkout(&dir.path().join(".oak"))
            .page(None, 50)
            .unwrap();
        let mut digest = Sha256::new();
        digest.update(b"POST\0/base/api/owner/repo/push/staged-v1\0");
        digest.update(body);
        assert_eq!(
            page.pending_publications[0].request_identity,
            hex::encode(digest.finalize())
        );

        let ordinary_body = b"exact ordinary body";
        let ordinary = super::prepare_publication_attempt(
            dir.path(),
            "https://example.com/base",
            "owner/repo",
            super::PublicationAttemptInput {
                branch: Some(&branch),
                target: Some(&target),
                expected: None,
                force: false,
                transport: crate::publication_state::PublicationTransport::Ordinary,
                body: ordinary_body,
                request_url: "https://example.com/base/api/owner/repo/push",
                final_handoff: None,
            },
        )
        .unwrap()
        .unwrap();
        let page = crate::publication_state::AttemptStore::checkout(&dir.path().join(".oak"))
            .page(None, 50)
            .unwrap();
        let recorded = page
            .pending_publications
            .iter()
            .find(|attempt| {
                attempt.transport == crate::publication_state::PublicationTransport::Ordinary
            })
            .unwrap();
        let mut digest = Sha256::new();
        digest.update(b"POST\0/base/api/owner/repo/push\0");
        digest.update(ordinary_body);
        assert_eq!(recorded.request_identity, hex::encode(digest.finalize()));
        drop(handle);
        drop(ordinary);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_final_handoff_is_unconfirmed_and_not_abortable() {
        let dir = tempfile::tempdir().unwrap();
        crate::commands::init::run(dir.path(), false).unwrap();
        let target = Hash("a".repeat(64));
        let branch = super::BranchData {
            name: "topic".to_string(),
            description: None,
            parent_branch: Some("main".to_string()),
            status: "open".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            close_reason: None,
        };
        let handoff = super::StagedFinalHandoff::default();
        let handle = super::prepare_publication_attempt(
            dir.path(),
            "https://example.com",
            "owner/repo",
            super::PublicationAttemptInput {
                branch: Some(&branch),
                target: Some(&target),
                expected: None,
                force: false,
                transport: crate::publication_state::PublicationTransport::StagedFinal,
                body: b"final body",
                request_url: "https://example.com/api/owner/repo/push/staged-v1",
                final_handoff: Some(&handoff),
            },
        )
        .unwrap()
        .unwrap();
        let operation_id = handle.operation_id().to_string();
        drop(handle);

        let result = super::await_staged_operation(
            std::future::pending::<oak_core::Result<()>>(),
            std::future::ready(Ok(())),
            &handoff,
        )
        .await;
        let error = result.as_ref().unwrap_err();
        assert!(matches!(error, OakError::PublicationUnconfirmed { .. }));
        assert!(error.to_string().contains(&operation_id));
        assert!(!super::should_abort_staged_session(&result, &handoff));
        let page = crate::publication_state::AttemptStore::checkout(&dir.path().join(".oak"))
            .page(None, 50)
            .unwrap();
        assert_eq!(page.pending_publication_count, 1);
        assert_eq!(
            page.pending_publications[0].state,
            crate::publication_state::AttemptState::SentUnconfirmed
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_publication_terminal_ambiguity_is_typed_and_redacted() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(502)
                    .set_body_string("SYNTHETIC_PRIVATE_ERROR_MARKER access_token=do-not-print"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let commands = vec!["oak branch show feature --remote --json".to_string()];
        let result = send_staged_publication_with_cap(
            crate::http::api_client().post(server.uri()),
            b"{}".to_vec(),
            std::time::Duration::from_secs(1),
            &commands,
            None,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("terminal ambiguous staged response must be typed"),
            Err(error) => error,
        };

        assert!(matches!(error, OakError::PublicationUnconfirmed { .. }));
        let message = error.to_string();
        assert!(!message.contains("SYNTHETIC_PRIVATE_ERROR_MARKER"));
        assert!(!message.contains("access_token"));
        assert!(!message.contains("retry the push"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_publication_first_rejection_is_redacted_and_not_retried() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("SYNTHETIC_PRIVATE_ERROR_MARKER access_token=do-not-print"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = send_staged_publication_with_cap(
            crate::http::api_client().post(server.uri()),
            b"{}".to_vec(),
            std::time::Duration::from_secs(1),
            &[],
            None,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("explicit staged rejection must fail"),
            Err(error) => error,
        };

        assert!(matches!(error, OakError::Server(_)));
        let message = error.to_string();
        assert!(message.contains("400 Bad Request"));
        assert!(!message.contains("SYNTHETIC_PRIVATE_ERROR_MARKER"));
        assert!(!message.contains("access_token"));
    }

    #[test]
    fn publication_ack_requires_a_native_lowercase_hash_when_non_null() {
        let response = PushResponse {
            success: true,
            new_head: Some("a".repeat(40)),
            message: String::new(),
            view: None,
        };
        assert!(validate_successful_publication_ack(&response, None, "metadata", &[]).is_err());

        let response = PushResponse {
            success: true,
            new_head: Some("A".repeat(64)),
            message: String::new(),
            view: None,
        };
        assert!(validate_successful_publication_ack(&response, None, "metadata", &[]).is_err());

        let response = PushResponse {
            success: true,
            new_head: Some("a".repeat(64)),
            message: String::new(),
            view: None,
        };
        validate_successful_publication_ack(&response, None, "metadata", &[]).unwrap();
    }

    #[test]
    fn publication_reconciliation_quotes_unsafe_branch_names_and_fences_remote_overrides() {
        let (_dir, repo) = temp_repo();
        repo.set_metadata(MetadataKey::RemoteUrl, "https://configured.example")
            .unwrap();

        let (commands, note) = publication_reconciliation_context(
            &repo,
            "https://configured.example",
            Some("topic; printf private"),
        )
        .unwrap();
        assert_eq!(
            commands,
            ["oak branch show --remote --json -- 'topic; printf private'"]
        );
        assert!(note.is_none());

        let (commands, note) =
            publication_reconciliation_context(&repo, "https://override.example", Some("feature"))
                .unwrap();
        assert_eq!(commands, ["oak info --json"]);
        let note = note.expect("a non-persisted override needs an explicit warning");
        assert!(note.contains("different server"));
        assert!(!note.contains("configured.example"));
        assert!(!note.contains("override.example"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_publication_rejects_an_oversized_success_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let oversized = serde_json::to_vec(&PushResponse {
            success: true,
            new_head: None,
            message: "x".repeat(STAGED_PUBLICATION_RESPONSE_MAX_BYTES),
            view: None,
        })
        .unwrap();
        assert!(oversized.len() > STAGED_PUBLICATION_RESPONSE_MAX_BYTES);
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/push/staged-v1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_bytes(oversized),
            )
            .expect(1)
            .mount(&server)
            .await;

        let result = send_staged_publication_with_cap(
            crate::http::api_client().post(format!("{}/api/oak/oak/push/staged-v1", server.uri())),
            serde_json::to_vec(&serde_json::json!({"stage_id": "same-request"})).unwrap(),
            std::time::Duration::from_secs(1),
            &[],
            None,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("oversized staged response must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, OakError::PublicationUnconfirmed { .. }));
        assert!(error.to_string().contains("complete bounded receipt"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_mapping_proof_polls_the_opaque_job_to_exact_completion() {
        use oak_core::protocol::{BlobProofChunk, BlobProofDescriptor};
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let chunks = vec![BlobProofChunk {
            hash: "c".repeat(64),
            offset: 0,
            size: 3,
        }];
        let descriptor = BlobProofDescriptor {
            hash: "a".repeat(64),
            size: 3,
            mapping_digest: oak_core::protocol::blob_mapping_digest(&chunks),
            total_chunks: 1,
        };
        let mapping = PreparedMappingProof {
            descriptor: descriptor.clone(),
            chunks: chunks.clone(),
        };
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/async-v1"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(serde_json::json!({
                "blobs": [descriptor.clone()]
            })))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "verified": [],
                "missing": [],
                "mapping_proof_job": {
                    "token": "opaque/token",
                    "status": "uploading",
                    "retry_after_ms": 1
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/oak/oak/blobs/proofs/opaque%2Ftoken/mappings"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(serde_json::json!({
                "pages": [{
                    "blob_index": 0,
                    "first_chunk_index": 0,
                    "chunks": chunks
                }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accepted_chunks": 1,
                "complete_blobs": [0],
                "all_mappings_complete": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/opaque%2Ftoken/finalize"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(serde_json::json!({})))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "verified": [],
                "missing": [],
                "mapping_proof_job": {
                    "token": "opaque/token",
                    "status": "pending",
                    "retry_after_ms": 1
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/blobs/proofs/opaque%2Ftoken"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "verified": [descriptor.hash.clone()],
                "missing": [],
                "proof_token": "opaque/token",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = prove_mapping_set_once(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            Some("secret"),
            std::slice::from_ref(&mapping),
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(token, "opaque/token");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_mapping_proof_restarts_a_stale_create_with_the_exact_request() {
        use oak_core::protocol::{BlobProofChunk, BlobProofDescriptor};
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let chunks = vec![BlobProofChunk {
            hash: "c".repeat(64),
            offset: 0,
            size: 3,
        }];
        let descriptor = BlobProofDescriptor {
            hash: "a".repeat(64),
            size: 3,
            mapping_digest: oak_core::protocol::blob_mapping_digest(&chunks),
            total_chunks: 1,
        };
        let request = serde_json::json!({ "blobs": [descriptor.clone()] });
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/async-v1"))
            .and(body_json(request.clone()))
            .respond_with(ResponseTemplate::new(409))
            .with_priority(1)
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/async-v1"))
            .and(body_json(request))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "verified": [descriptor.hash.clone()],
                "missing": [],
                "proof_token": "replacement-token"
            })))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        let uploaded = upload_mapping_set_with_restarts(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            None,
            &[PreparedMappingProof {
                descriptor: descriptor.clone(),
                chunks,
            }],
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        let token = uploaded.terminal_token.unwrap();
        assert_eq!(token, "replacement-token");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_mapping_generation_conflict_is_not_retried() {
        use oak_core::protocol::{BlobProofChunk, BlobProofDescriptor};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/async-v1"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT
            })))
            .expect(1)
            .mount(&server)
            .await;
        let chunks = vec![BlobProofChunk {
            hash: "c".repeat(64),
            offset: 0,
            size: 3,
        }];
        let result = upload_mapping_set_with_restarts(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            None,
            &[PreparedMappingProof {
                descriptor: BlobProofDescriptor {
                    hash: "a".repeat(64),
                    size: 3,
                    mapping_digest: oak_core::protocol::blob_mapping_digest(&chunks),
                    total_chunks: 1,
                },
                chunks,
            }],
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("terminal generation conflict must fail closed"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains(oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_mapping_pages_preserve_header_order_and_split_at_ten_thousand_refs() {
        use oak_core::protocol::{BlobProofChunk, BlobProofDescriptor};
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let first_chunks = vec![BlobProofChunk {
            hash: "c".repeat(64),
            offset: 0,
            size: 1,
        }];
        let second_chunks: Vec<BlobProofChunk> = (0..10_001)
            .map(|offset| BlobProofChunk {
                hash: "d".repeat(64),
                offset,
                size: 1,
            })
            .collect();
        let mappings = vec![
            PreparedMappingProof {
                descriptor: BlobProofDescriptor {
                    hash: "a".repeat(64),
                    size: 1,
                    mapping_digest: oak_core::protocol::blob_mapping_digest(&first_chunks),
                    total_chunks: 1,
                },
                chunks: first_chunks.clone(),
            },
            PreparedMappingProof {
                descriptor: BlobProofDescriptor {
                    hash: "b".repeat(64),
                    size: 10_001,
                    mapping_digest: oak_core::protocol::blob_mapping_digest(&second_chunks),
                    total_chunks: 10_001,
                },
                chunks: second_chunks.clone(),
            },
        ];
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/proofs/async-v1"))
            .and(body_json(serde_json::json!({
                "blobs": [mappings[0].descriptor.clone(), mappings[1].descriptor.clone()]
            })))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "verified": [],
                "missing": [],
                "mapping_proof_job": {
                    "token": "ordered-token",
                    "status": "uploading",
                    "retry_after_ms": 1
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mapping_path = "/api/oak/oak/blobs/proofs/ordered-token/mappings";
        for (blob_index, first_chunk_index, chunks) in [
            (0u32, 0u32, first_chunks),
            (1, 0, second_chunks[..10_000].to_vec()),
            (1, 10_000, second_chunks[10_000..].to_vec()),
        ] {
            let accepted_chunks = chunks.len() as u32;
            Mock::given(method("PUT"))
                .and(path(mapping_path))
                .and(body_json(serde_json::json!({
                    "pages": [{
                        "blob_index": blob_index,
                        "first_chunk_index": first_chunk_index,
                        "chunks": chunks
                    }]
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "accepted_chunks": accepted_chunks,
                    "complete_blobs": [],
                    "all_mappings_complete": false
                })))
                .expect(1)
                .mount(&server)
                .await;
        }

        let uploaded = upload_mapping_set_once(
            &crate::http::api_client(),
            &server.uri(),
            "oak/oak",
            None,
            &mappings,
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(uploaded.job.token, "ordered-token");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mapping_proof_deadline_bounds_a_peer_that_never_returns_headers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proof"))
            .respond_with(ResponseTemplate::new(202).set_delay(std::time::Duration::from_secs(5)))
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        let error = send_mapping_proof_request_with_cap(
            crate::http::api_client().post(format!("{}/proof", server.uri())),
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(25),
        )
        .await
        .unwrap_err();

        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(error.to_string().contains("deadline"));
    }

    #[test]
    fn parse_remote_identity_returns_valid_owner_and_repo() {
        let linked = parse_remote_identity("oak/benchmarks").unwrap();
        assert_eq!(linked, ("oak".to_string(), "benchmarks".to_string()));
    }

    #[test]
    fn parse_remote_identity_rejects_unsafe_specs() {
        for spec in [
            "oak",
            "/repo",
            "oak/",
            "./repo",
            "../repo",
            "oak/.",
            "oak/..",
            "oak\\team/repo",
            "oak/repo\\path",
            "oak team/repo",
            "oak/repo name",
            "oak/repo/subtree",
        ] {
            assert!(
                parse_remote_identity(spec).is_err(),
                "expected {spec:?} to be rejected"
            );
        }
    }

    #[test]
    fn window_budget_preserves_legacy_and_other_cumulative_caps() {
        let totals = PushOperationTotals {
            resolved_manifest_entries: 1_005_000,
            window_resolved_entries: 1_007_010,
            blobs: 50_001,
            ..Default::default()
        };
        assert!(validate_push_operation_caps(totals).is_err());
        assert!(super::validate_push_operation_caps_for_budget(totals, true).is_ok());
        let exact = PushOperationTotals {
            window_resolved_entries: super::STAGED_WINDOW_MAX_RESOLVED_MANIFEST_ENTRIES,
            blobs: super::STAGED_WINDOW_MAX_BLOBS,
            ..Default::default()
        };
        assert!(super::validate_push_operation_caps_for_budget(exact, true).is_ok());
        for over in [
            PushOperationTotals {
                window_resolved_entries: exact.window_resolved_entries + 1,
                ..exact
            },
            PushOperationTotals {
                blobs: exact.blobs + 1,
                ..exact
            },
            PushOperationTotals {
                canonical_metadata_bytes: oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES
                    + 1,
                ..exact
            },
        ] {
            assert!(super::validate_push_operation_caps_for_budget(over, true).is_err());
        }
    }

    fn planned_blob(index: u32, size: u64, chunk_refs: usize) -> super::PlannedBlob {
        super::PlannedBlob {
            hash: oak_core::hash_bytes(&index.to_be_bytes()),
            size,
            chunk_refs,
        }
    }

    #[test]
    fn ordinary_preload_batches_stay_inside_the_ordinary_envelope() {
        let sizes = |batches: &[Vec<super::PlannedBlob>]| -> Vec<usize> {
            batches.iter().map(Vec::len).collect()
        };
        let small: Vec<_> = (0..1000).map(|index| planned_blob(index, 50, 0)).collect();
        let batches = super::split_ordinary_preload_batches(small).unwrap();
        assert_eq!(sizes(&batches), [128, 128, 128, 128, 128, 128, 128, 104]);

        // 60 MiB + 60 MiB + 60 MiB: never two in one 64 MiB request.
        let sixty = 60 * 1024 * 1024;
        let batches = super::split_ordinary_preload_batches(
            (0..3).map(|index| planned_blob(index, sixty, 12)).collect(),
        )
        .unwrap();
        assert_eq!(sizes(&batches), [1, 1, 1]);

        // One blob above 64 MiB travels alone; its neighbours do not join it.
        let batches = super::split_ordinary_preload_batches(vec![
            planned_blob(0, 10, 0),
            planned_blob(1, 200 * 1024 * 1024, 40),
            planned_blob(2, 10, 0),
        ])
        .unwrap();
        assert_eq!(sizes(&batches), [1, 1, 1]);
        assert_eq!(batches[1][0].size, 200 * 1024 * 1024);

        // Chunk references are bounded per request, and one blob that alone
        // exceeds a request's chunk-reference page is not preloadable.
        let batches = super::split_ordinary_preload_batches(vec![
            planned_blob(0, 1, 6_000),
            planned_blob(1, 1, 6_000),
        ])
        .unwrap();
        assert_eq!(sizes(&batches), [1, 1]);
        let refusal =
            super::split_ordinary_preload_batches(vec![planned_blob(0, 1, 10_001)]).unwrap_err();
        assert!(
            refusal.contains("10001 chunk references > 10000"),
            "{refusal}"
        );
    }

    #[test]
    fn capability_refusal_names_each_measured_total_and_its_limit() {
        let refusal = |totals, capability| match super::staged_required_unavailable_error(
            totals, capability, None,
        ) {
            OakError::PushCapabilityUnavailable {
                message,
                recommended_next_commands,
            } => {
                assert_eq!(recommended_next_commands, ["oak push --plan --json"]);
                assert!(message.contains("requires staged_v1"), "{message}");
                assert!(
                    message.ends_with("no remote state was mutated"),
                    "{message}"
                );
                message
            }
            other => panic!("{other:?}"),
        };
        let message = refusal(
            PushOperationTotals {
                commits: 501,
                blobs: 129,
                chunk_refs: 10_001,
                declared_blob_bytes: 64 * 1024 * 1024 + 1,
                ..Default::default()
            },
            Some(super::ServerPushCapability::PhaseOneOrdinaryBootstrap),
        );
        for needle in [
            "501 outgoing commits > 500",
            "129 new blobs > 128",
            "67108865 new blob bytes > 67108864 (64 MiB)",
            "10001 new chunk references > 10000",
            "lifts only the new-blob count and byte limits",
        ] {
            assert!(message.contains(needle), "missing {needle:?}: {message}");
        }
        let message = refusal(
            PushOperationTotals {
                blobs: 129,
                ..Default::default()
            },
            Some(super::ServerPushCapability::Legacy),
        );
        assert!(
            message.contains("does not advertise ordinary_bootstrap_protocol=headless_preload_v1")
        );
    }

    #[tokio::test]
    async fn blob_preload_is_selected_only_without_staged_and_only_for_blob_totals() {
        use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
        let headless = serde_json::json!({"push_protocol":"legacy","staged_capabilities_ready":false,"staged_session_protocol":"opaque_v1","staged_abort_protocol":"v1","known_loss_protocol":oak_core::protocol::KNOWN_LOSS_PROTOCOL,"ordinary_bootstrap_protocol":"headless_preload_v1"});
        let staged = serde_json::json!({"push_protocol":"staged_v1","staged_capabilities_ready":true,"staged_session_protocol":"opaque_v1","mapping_proof_protocol":"async_v1","staged_abort_protocol":"v1","ordinary_bootstrap_protocol":"headless_preload_v1"});
        let legacy =
            serde_json::json!({"push_protocol":"legacy","staged_capabilities_ready":false});
        let blobs = PushOperationTotals {
            blobs: 129,
            ..Default::default()
        };
        let bytes = PushOperationTotals {
            blobs: 1,
            declared_blob_bytes: 100 * 1024 * 1024,
            ..Default::default()
        };
        let commits = PushOperationTotals {
            commits: 501,
            blobs: 129,
            ..Default::default()
        };
        use super::PushTransport::*;
        for (capabilities, totals, first_publication, existing_repo, expected) in [
            (&headless, blobs, false, true, OrdinaryBlobPreload),
            (&headless, bytes, false, true, OrdinaryBlobPreload),
            (&headless, commits, false, true, StagedRequiredUnavailable),
            (&headless, blobs, false, false, StagedRequiredUnavailable),
            // The first-publication bridge keeps its precedence.
            (&headless, blobs, true, true, PhaseOneOrdinaryBootstrap),
            // staged_v1 stays preferred whenever the server has it.
            (&staged, blobs, false, true, StagedReady),
            (&staged, bytes, true, true, StagedReady),
            (&legacy, blobs, false, true, StagedRequiredUnavailable),
            (&legacy, bytes, true, true, StagedRequiredUnavailable),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/api/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(capabilities.clone()))
                .expect(1)
                .mount(&server)
                .await;
            let (transport, _) = super::select_push_transport(
                &crate::http::api_client(),
                &server.uri(),
                None,
                totals,
                first_publication,
                existing_repo,
            )
            .await
            .unwrap();
            assert_eq!(transport, expected, "{capabilities} {totals:?}");
        }
    }

    #[tokio::test]
    async fn extended_admission_requires_exact_complete_window_capability() {
        use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
        for (protocol, ready, allowed) in [
            ("window_v1", true, true),
            ("unknown", true, false),
            ("", true, false),
            ("window_v1", false, false),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/api/capabilities")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"push_protocol":"staged_v1","staged_capabilities_ready":ready,"staged_session_protocol":"opaque_v1","mapping_proof_protocol":"async_v1","staged_abort_protocol":"v1","staged_budget_protocol":protocol}))).expect(1).mount(&server).await;
            let totals = PushOperationTotals {
                blobs: 50_001,
                ..Default::default()
            };
            let result = super::select_push_transport(
                &crate::http::api_client(),
                &server.uri(),
                None,
                totals,
                false,
                false,
            )
            .await;
            if allowed {
                assert_eq!(result.unwrap().0, super::PushTransport::StagedWindowReady);
            } else {
                assert!(result.is_err());
            }
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "admission must not mutate remote state"
            );
        }
    }

    #[test]
    fn window_batches_bound_201_five_thousand_file_roots_and_parent_work() {
        for placements in [5000usize, 5010] {
            let (_dir, repo) = temp_repo();
            let mut commits = Vec::new();
            let mut costs = std::collections::HashMap::new();
            let mut paths = std::collections::HashMap::new();
            let mut parent = None;
            for index in 0..201 {
                let root = hash_bytes(format!("root{index}").as_bytes());
                let commit = Commit::new(
                    "main".into(),
                    parent,
                    None,
                    root.clone(),
                    "test".into(),
                    None,
                    vec![],
                )
                .unwrap();
                repo.store_commit(&commit).unwrap();
                costs.insert(root.clone(), placements);
                paths.insert(root, 50_000);
                parent = Some(commit.hash.clone());
                commits.push(commit);
            }
            let original: Vec<_> = commits.iter().map(|c| c.hash.clone()).collect();
            let batches =
                super::split_window_commit_batches(&repo, commits, &mut costs, &paths).unwrap();
            assert_eq!(
                batches.iter().map(Vec::len).collect::<Vec<_>>(),
                if placements == 5000 {
                    vec![200, 1]
                } else {
                    vec![199, 2]
                }
            );
            for batch in &batches {
                let mut roots = std::collections::HashSet::new();
                for c in batch {
                    roots.insert(c.manifest_hash.clone());
                    if let Some(parent) = &c.parent_hash {
                        roots.insert(repo.get_commit(parent).unwrap().unwrap().manifest_hash);
                    }
                }
                assert!(roots.iter().map(|r| costs[r]).sum::<usize>() <= 1_000_000);
            }
            assert_eq!(
                batches
                    .into_iter()
                    .flatten()
                    .map(|c| c.hash)
                    .collect::<Vec<_>>(),
                original
            );
        }
    }

    #[test]
    fn window_validation_counts_directory_edges_and_repeated_subtree_placements() {
        let (_dir, repo) = temp_repo();
        let blob = repo.put_blob(b"x".to_vec()).unwrap();
        let leaf = Tree::new(vec![TreeEntry {
            name: "f".into(),
            kind: TreeEntryKind::Blob,
            hash: blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
        repo.store_tree(&leaf).unwrap();
        let root = Tree::new(
            ["a", "b"]
                .into_iter()
                .map(|name| TreeEntry {
                    name: name.into(),
                    kind: TreeEntryKind::Tree,
                    hash: leaf.hash.clone(),
                    mode: FileMode::Regular,
                })
                .collect(),
        )
        .unwrap();
        repo.store_tree(&root).unwrap();
        assert_eq!(
            super::validation_cost_for_root(&repo, &root.hash, &mut Default::default()).unwrap(),
            4
        );
    }

    #[test]
    fn whole_operation_caps_reject_every_dimension_before_protocol_selection() {
        let cases = [
            (
                "commits",
                PushOperationTotals {
                    commits: oak_core::protocol::STAGED_OPERATION_MAX_COMMITS + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "tree objects",
                PushOperationTotals {
                    trees: oak_core::protocol::STAGED_MAX_TREE_OBJECTS + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "direct tree entries",
                PushOperationTotals {
                    direct_tree_entries: oak_core::protocol::STAGED_MAX_DIRECT_TREE_ENTRIES + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "resolved manifest entries",
                PushOperationTotals {
                    resolved_manifest_entries:
                        oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "file changes",
                PushOperationTotals {
                    file_changes: oak_core::protocol::STAGED_MAX_FILE_CHANGES + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "canonical metadata bytes",
                PushOperationTotals {
                    canonical_metadata_bytes:
                        oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "expanded path bytes",
                PushOperationTotals {
                    expanded_path_bytes: oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "chunk references",
                PushOperationTotals {
                    chunk_refs: oak_core::protocol::STAGED_MAX_CHUNK_REFS + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "blobs",
                PushOperationTotals {
                    blobs: oak_core::protocol::STAGED_MAX_BLOBS + 1,
                    ..PushOperationTotals::default()
                },
            ),
            (
                "declared blob bytes",
                PushOperationTotals {
                    declared_blob_bytes: oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES + 1,
                    ..PushOperationTotals::default()
                },
            ),
        ];
        for (dimension, totals) in cases {
            let error = validate_push_operation_caps(totals)
                .expect_err("cap+1 must fail admission before remote work");
            assert!(error.to_string().contains(dimension), "{error}");
            assert!(error.to_string().contains("No remote state was mutated"));
        }

        let operation_overflow = PushOperationTotals {
            trees: oak_core::protocol::STAGED_MAX_TREE_OBJECTS + 1,
            ..PushOperationTotals::default()
        };
        assert!(
            !staged_protocol_required(operation_overflow),
            "operation overflow must not be mistaken for an envelope requiring staging"
        );
        assert!(staged_protocol_required(PushOperationTotals {
            commits: oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS + 1,
            ..PushOperationTotals::default()
        }));
    }

    #[test]
    fn staged_blob_plan_splits_before_the_129th_async_proof_header() {
        let blobs = (0..129)
            .map(|index| PlannedBlob {
                hash: Hash(format!("{index:064x}")),
                size: 1,
                chunk_refs: 1,
            })
            .collect();

        let sets = split_staged_blob_batches(blobs).unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].len(), 128);
        assert_eq!(sets[1].len(), 1);
    }

    #[test]
    fn ordinary_push_is_not_rejected_by_staged_operation_aggregate_caps() {
        let legacy_ordinary = PushOperationTotals {
            commits: 1,
            trees: oak_core::protocol::STAGED_MAX_TREE_OBJECTS + 1,
            resolved_manifest_entries: oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES + 1,
            ..PushOperationTotals::default()
        };
        assert!(!select_staged_protocol(legacy_ordinary).unwrap());

        let staged = PushOperationTotals {
            commits: oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS + 1,
            ..legacy_ordinary
        };
        assert!(select_staged_protocol(staged).is_err());
    }

    #[test]
    fn one_commit_multi_gib_or_paged_mapping_selects_staged_async_proof() {
        let large = PushOperationTotals {
            commits: 1,
            blobs: 1,
            declared_blob_bytes: 5 * 1024 * 1024 * 1024,
            chunk_refs: 2_048,
            ..PushOperationTotals::default()
        };
        assert!(select_staged_protocol(large).unwrap());

        let small_chunked = PushOperationTotals {
            commits: 1,
            blobs: 1,
            declared_blob_bytes: 1024,
            chunk_refs: 1,
            ..PushOperationTotals::default()
        };
        assert!(!select_staged_protocol(small_chunked).unwrap());

        let small_inline = PushOperationTotals {
            commits: 1,
            blobs: 1,
            declared_blob_bytes: 1024,
            chunk_refs: 0,
            ..PushOperationTotals::default()
        };
        assert!(!select_staged_protocol(small_inline).unwrap());

        let paged_mapping = PushOperationTotals {
            commits: 1,
            blobs: 1,
            declared_blob_bytes: 32 * 1024 * 1024,
            chunk_refs: oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS + 1,
            ..PushOperationTotals::default()
        };
        assert!(select_staged_protocol(paged_mapping).unwrap());
    }

    #[test]
    fn outgoing_plan_is_topological_across_batch_boundary_despite_skewed_timestamps() {
        let (_dir, repo) = temp_repo();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut parent = None;
        for index in 0..=BOOTSTRAP_BATCH_SIZE {
            let commit = Commit::with_timestamp(
                "main".to_string(),
                parent,
                None,
                oak_core::Tree::empty_hash(),
                "importer".to_string(),
                None,
                Vec::new(),
                base - chrono::Duration::seconds(index as i64),
            )
            .unwrap();
            repo.store_commit(&commit).unwrap();
            parent = Some(commit.hash);
        }
        repo.set_branch_head("main", parent.as_ref().unwrap())
            .unwrap();

        let plan = plan_outgoing_commits(
            &repo,
            "main",
            parent.as_ref(),
            None,
            &std::collections::HashSet::new(),
        )
        .unwrap();
        assert_eq!(plan.len(), BOOTSTRAP_BATCH_SIZE + 1);
        for pair in plan.windows(2) {
            assert_eq!(pair[1].parent_hash.as_ref(), Some(&pair[0].hash));
        }
        assert_eq!(
            plan[BOOTSTRAP_BATCH_SIZE].parent_hash.as_ref(),
            Some(&plan[BOOTSTRAP_BATCH_SIZE - 1].hash),
            "batch two must begin by extending batch one's published head"
        );
    }

    #[test]
    fn planned_objects_share_one_tree_scan_across_many_commits() {
        const FILES: usize = 5_000;
        const COMMITS: usize = 200;
        let (_dir, repo) = temp_repo();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        let blob_hash = repo.put_blob(b"shared\n".to_vec()).unwrap();
        let root = repo
            .put_manifest(
                (0..FILES)
                    .map(|index| ManifestEntry {
                        path: format!("files/{index:05}.txt"),
                        blob_hash: blob_hash.clone(),
                        mode: FileMode::Regular,
                    })
                    .collect(),
            )
            .unwrap();
        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut parent = None;
        for index in 0..COMMITS {
            let commit = Commit::with_timestamp(
                "main".to_string(),
                parent,
                None,
                root.clone(),
                "importer".to_string(),
                None,
                Vec::new(),
                base + chrono::Duration::seconds(index as i64),
            )
            .unwrap();
            repo.store_commit(&commit).unwrap();
            parent = Some(commit.hash);
        }
        repo.set_branch_head("main", parent.as_ref().unwrap())
            .unwrap();

        let started = std::time::Instant::now();
        let plan = plan_outgoing_commits(
            &repo,
            "main",
            parent.as_ref(),
            None,
            &std::collections::HashSet::new(),
        )
        .unwrap();
        let plan_refs: Vec<&Commit> = plan.iter().collect();
        let objects = collect_planned_objects(&repo, &plan_refs).unwrap();
        eprintln!(
            "admitted {COMMITS} commits sharing {FILES} paths in {:?}",
            started.elapsed()
        );
        assert_eq!(plan.len(), COMMITS);
        assert!(objects.missing_blobs.is_empty());
    }

    fn pseudo_random(len: usize, mut state: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xFF) as u8
            })
            .collect()
    }

    /// The one mapping of `content`, as wire references.
    fn one_mapping(content: &[u8]) -> Vec<(String, u64, u32)> {
        crate::transfer::map_blob(content)
            .unwrap()
            .into_iter()
            .map(|(chunk, _)| (chunk.hash, chunk.offset, chunk.size))
            .collect()
    }

    fn wire_mapping(blob: &BlobData) -> Vec<(String, u64, u32)> {
        blob.chunks
            .iter()
            .map(|chunk| (chunk.hash.clone(), chunk.offset, chunk.size))
            .collect()
    }

    /// Store `content` as a blob whose bytes live only in `parts`-shaped
    /// local chunks (how a pulled or legacy-pushed blob is held).
    fn store_blob_as_chunks(repo: &SqliteRepository, content: &[u8], cuts: &[usize]) -> Hash {
        let blob_hash = hash_bytes(content);
        let mut mapping = Vec::new();
        let mut offset = 0usize;
        for &length in cuts {
            let bytes = &content[offset..offset + length];
            let hash = hash_bytes(bytes);
            repo.store_chunk(&hash, bytes).unwrap();
            mapping.push(ChunkInfo {
                hash,
                offset: offset as u64,
                length: length as u32,
            });
            offset += length;
        }
        assert_eq!(offset, content.len());
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content: Vec::new(),
            size: content.len() as u64,
        })
        .unwrap();
        repo.store_blob_chunks(&blob_hash, &mapping).unwrap();
        blob_hash
    }

    fn planned(hash: &Hash, size: usize) -> PlannedBlob {
        PlannedBlob {
            hash: hash.clone(),
            size: size as u64,
            chunk_refs: 0,
        }
    }

    /// The mapping a prepared batch will put on the wire for `hash`: the
    /// batch must carry the blob as exactly one deferred source with no
    /// inline bytes, and that source is cut by `local_blob_mapping` — the
    /// same stream `upload_outgoing_chunks` uploads from.
    fn deferred_wire_mapping(
        repo: &SqliteRepository,
        blobs: &[BlobData],
        sources: &[PreparedChunk],
        hash: &Hash,
    ) -> Vec<(String, u64, u32)> {
        let blob = blobs
            .iter()
            .find(|blob| blob.hash == hash.as_str())
            .expect("the blob is part of the batch");
        assert!(blob.content.is_empty(), "a large blob never travels inline");
        let deferred: Vec<u64> = sources
            .iter()
            .filter_map(|source| match source {
                PreparedChunk::Blob { hash: source, size } if source == hash => Some(*size),
                _ => None,
            })
            .collect();
        assert_eq!(deferred, vec![blob.size], "exactly one deferred source");
        local_blob_mapping(repo, hash, blob.size)
            .unwrap()
            .into_iter()
            .map(|chunk| (chunk.hash, chunk.offset, chunk.size))
            .collect()
    }

    /// The invariant: a blob's chunk mapping is a pure function of its
    /// bytes. Every transport's blob preparation — ordinary collection,
    /// headless preload, phase-one bootstrap, staged — and a blob held
    /// locally in any earlier shape must put exactly `transfer::map_blob`'s
    /// mapping on the wire. The wire itself is pinned per transport in
    /// `push_blob_preload::a_large_file_has_one_chunk_mapping_on_every_ordinary_wire_transport`.
    #[test]
    fn every_transport_sends_the_one_chunk_mapping_for_the_same_bytes() {
        let large = pseudo_random(
            oak_core::LARGE_FILE_THRESHOLD as usize + 1_234_567,
            0xC0FFEE,
        );
        let expected = one_mapping(&large);
        assert!(expected.len() > 1, "a large blob is never one chunk");
        assert!(expected.iter().all(|(_, _, size)| *size <= 4 * 1024 * 1024));

        // Ordinary push: collection admits the blob without loading it and
        // `map_outgoing_blobs` shapes whatever the server still lacks.
        let (_ordinary_dir, ordinary_repo) = temp_repo();
        let hash = ordinary_repo.put_blob(large.clone()).unwrap();
        let mut ordinary = vec![admit_outgoing_blob(&ordinary_repo, &hash, false)
            .unwrap()
            .unwrap()];
        let mut ordinary_sources = Vec::new();
        map_outgoing_blobs(&mut ordinary, &mut ordinary_sources).unwrap();
        assert_eq!(
            deferred_wire_mapping(&ordinary_repo, &ordinary, &ordinary_sources, &hash),
            expected,
            "ordinary"
        );

        // Headless preload: the blob is committed locally as whole content.
        let (_preload_dir, preload_repo) = temp_repo();
        let hash = preload_repo.put_blob(large.clone()).unwrap();
        let mut preload =
            materialize_planned_blob_batch(&preload_repo, vec![planned(&hash, large.len())])
                .unwrap();
        map_outgoing_blobs(&mut preload.blobs, &mut preload.chunks).unwrap();
        assert_eq!(
            deferred_wire_mapping(&preload_repo, &preload.blobs, &preload.chunks, &hash),
            expected,
            "headless preload"
        );

        // Phase-one bootstrap and staged: operation-wide preflight, then each
        // batch is rematerialized for execution; staged additionally proves
        // the mapping it is about to send.
        let (_staged_dir, staged_repo) = temp_repo();
        let hash = staged_repo.put_blob(large.clone()).unwrap();
        let batches =
            super::preflight_staged_blob_batches(&staged_repo, vec![planned(&hash, large.len())])
                .unwrap();
        assert_eq!(batches.len(), 1);
        let mut bootstrap =
            materialize_planned_blob_batch(&staged_repo, batches[0].clone()).unwrap();
        map_outgoing_blobs(&mut bootstrap.blobs, &mut bootstrap.chunks).unwrap();
        assert_eq!(
            deferred_wire_mapping(&staged_repo, &bootstrap.blobs, &bootstrap.chunks, &hash),
            expected,
            "bootstrap"
        );
        let proofs = prepare_staged_mapping_proofs(&staged_repo, &mut bootstrap.blobs).unwrap();
        assert_eq!(wire_mapping(&bootstrap.blobs[0]), expected, "staged");
        assert_eq!(
            proofs[0]
                .chunks
                .iter()
                .map(|chunk| (chunk.hash.clone(), chunk.offset, chunk.size))
                .collect::<Vec<_>>(),
            expected,
            "staged mapping proof"
        );
        for (name, repo) in [("preload", &preload_repo), ("staged", &staged_repo)] {
            assert!(
                repo.get_blob_chunks(&hash)
                    .unwrap()
                    .is_none_or(|mapping| mapping.is_empty()),
                "{name} must not write a second copy into the local store"
            );
        }

        // A blob held locally in the legacy whole-blob shape (what older
        // clients' preplanned transports stored) or in arbitrary cuts (a
        // server's advertisement) still goes out as the one mapping, and
        // the local store is left exactly as it was.
        for cuts in [
            vec![large.len()],
            vec![5_000_000, 5_000_000, large.len() - 10_000_000],
        ] {
            let (_dir, repo) = temp_repo();
            let hash = store_blob_as_chunks(&repo, &large, &cuts);
            assert!(admit_local_blob_descriptor(&repo, &hash).unwrap().is_some());
            let mut legacy =
                materialize_planned_blob_batch(&repo, vec![planned(&hash, large.len())]).unwrap();
            map_outgoing_blobs(&mut legacy.blobs, &mut legacy.chunks).unwrap();
            assert_eq!(
                deferred_wire_mapping(&repo, &legacy.blobs, &legacy.chunks, &hash),
                expected,
                "stored as {cuts:?}"
            );
            prepare_staged_mapping_proofs(&repo, &mut legacy.blobs).unwrap();
            assert_eq!(
                wire_mapping(&legacy.blobs[0]),
                expected,
                "staged, stored as {cuts:?}"
            );
            let stored: Vec<usize> = repo
                .get_blob_chunks(&hash)
                .unwrap()
                .unwrap()
                .iter()
                .map(|chunk| chunk.length as usize)
                .collect();
            assert_eq!(stored, cuts, "the local mapping is not rewritten");
        }

        // Below the threshold the one mapping is the blob itself: inline in
        // the request, or — when the inline total would approach the request
        // envelope — the same single chunk through the chunk endpoints.
        let inline = |content: &[u8]| BlobData {
            hash: hash_bytes(content).to_string(),
            content: content.to_vec(),
            size: content.len() as u64,
            chunks: Vec::new(),
            mapping_proof_token: None,
        };
        let small: Vec<Vec<u8>> = (0..4)
            .map(|index| pseudo_random(4 * 1024 * 1024 + index, 77 + index as u32))
            .collect();
        let mut alone = vec![inline(&small[0])];
        map_outgoing_blobs(&mut alone, &mut Vec::new()).unwrap();
        assert!(alone[0].chunks.is_empty());
        assert_eq!(alone[0].content, small[0]);
        let mut together: Vec<BlobData> = small.iter().map(|content| inline(content)).collect();
        let mut sources = Vec::new();
        map_outgoing_blobs(&mut together, &mut sources).unwrap();
        for (blob, content) in together.iter().zip(&small) {
            assert!(blob.content.is_empty());
            assert_eq!(wire_mapping(blob), one_mapping(content));
            assert_eq!(blob.chunks.len(), 1);
            assert_eq!(
                blob.chunks[0].hash, blob.hash,
                "a small blob is its own chunk"
            );
        }
        assert_eq!(sources.len(), small.len());
        assert!(sources
            .iter()
            .all(|source| matches!(source, PreparedChunk::Buffered(..))));
    }

    #[test]
    fn a_planned_large_blob_is_never_loaded_whole() {
        let (_dir, repo) = temp_repo();
        let content = pseudo_random(12 * 1024 * 1024, 0xABCD_EF01);
        let expected = one_mapping(&content);
        let blob_hash = repo.put_blob(content).unwrap();
        let tree = Tree::new(vec![TreeEntry {
            name: "large.bin".to_string(),
            kind: TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        repo.store_tree(&tree).unwrap();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            tree.hash,
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
        .unwrap();

        let plan = collect_planned_objects(&repo, &[&commit]).unwrap();
        assert_eq!(plan.blobs.len(), 1);
        let mut materialized = materialize_planned_blob_batch(&repo, plan.blobs).unwrap();
        assert!(materialized.blobs[0].content.is_empty());
        map_outgoing_blobs(&mut materialized.blobs, &mut materialized.chunks).unwrap();
        // The whole batch retains one descriptor for the 12 MiB blob: no
        // bytes, no per-chunk entries.
        assert!(matches!(
            materialized.chunks.as_slice(),
            [PreparedChunk::Blob { hash, size }] if *hash == blob_hash && *size == 12 * 1024 * 1024
        ));
        assert!(materialized.blobs[0].content.is_empty());
        assert!(materialized.blobs[0].chunks.is_empty());
        assert_eq!(
            deferred_wire_mapping(&repo, &materialized.blobs, &materialized.chunks, &blob_hash),
            expected
        );
    }

    #[test]
    fn a_corrupt_large_blob_is_rejected_by_the_stream_without_loading_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("oak.db");
        let repo = SqliteRepository::open(&db_path).unwrap();
        let size = oak_core::LARGE_FILE_THRESHOLD as usize + 4096;
        let hash = repo.put_blob(pseudo_random(size, 9)).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE blobs SET content = ?1, codec = 0 WHERE hash = ?2",
            rusqlite::params![pseudo_random(size, 10), hash.as_str()],
        )
        .unwrap();
        drop(conn);

        for error in [
            admit_local_blob_descriptor(&repo, &hash).unwrap_err(),
            admit_outgoing_blob(&repo, &hash, false).err().unwrap(),
            local_blob_mapping(&repo, &hash, size as u64).err().unwrap(),
        ] {
            let message = error.to_string();
            assert!(
                message.contains("push admission phase rejected local blob")
                    && message.contains("hash mismatch"),
                "got: {message}"
            );
        }
    }

    #[test]
    fn a_small_blob_held_as_chunks_is_sent_as_itself_and_left_as_stored() {
        let (_dir, repo) = temp_repo();
        let parts = [
            vec![b'a'; 300_000],
            vec![b'b'; 300_000],
            vec![b'c'; 300_000],
        ];
        let content: Vec<u8> = parts.iter().flatten().copied().collect();
        let blob_hash = store_blob_as_chunks(&repo, &content, &[300_000, 300_000, 300_000]);

        let mut objects =
            materialize_planned_blob_batch(&repo, vec![planned(&blob_hash, content.len())])
                .unwrap();
        map_outgoing_blobs(&mut objects.blobs, &mut objects.chunks).unwrap();

        // Below the large-file threshold the one mapping is the blob itself:
        // inline here, with nothing to prove and nothing written locally.
        assert_eq!(objects.blobs[0].content, content);
        assert!(objects.blobs[0].chunks.is_empty());
        assert!(objects.chunks.is_empty());
        assert!(prepare_staged_mapping_proofs(&repo, &mut objects.blobs)
            .unwrap()
            .is_empty());
        assert_eq!(repo.get_blob_chunks(&blob_hash).unwrap().unwrap().len(), 3);
    }

    #[test]
    fn planned_many_unique_trees_retain_only_hash_descriptors() {
        const LEAVES: usize = 2_000;
        let (_dir, repo) = temp_repo();
        let blob_hash = repo.put_blob(b"shared leaf bytes\n".to_vec()).unwrap();
        let mut root_entries = Vec::with_capacity(LEAVES);
        for index in 0..LEAVES {
            let leaf = Tree::new(vec![TreeEntry {
                name: format!("file-{index:05}.txt"),
                kind: TreeEntryKind::Blob,
                hash: blob_hash.clone(),
                mode: FileMode::Regular,
            }])
            .unwrap();
            repo.store_tree(&leaf).unwrap();
            root_entries.push(TreeEntry {
                name: format!("dir-{index:05}"),
                kind: TreeEntryKind::Tree,
                hash: leaf.hash,
                mode: FileMode::Regular,
            });
        }
        let root = Tree::new(root_entries).unwrap();
        repo.store_tree(&root).unwrap();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            root.hash,
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
        .unwrap();

        let plan = collect_planned_objects(&repo, &[&commit]).unwrap();
        assert_eq!(plan.tree_hashes.len(), LEAVES + 1);
        let descriptor_bytes = plan
            .tree_hashes
            .capacity()
            .saturating_mul(std::mem::size_of::<oak_core::Hash>());
        let wire_bytes = plan.tree_hashes.iter().fold(0usize, |total, hash| {
            let tree = repo.get_tree(hash).unwrap().unwrap();
            total.saturating_add(serde_json::to_vec(&tree_to_wire(&tree)).unwrap().len())
        });
        eprintln!(
            "planned {} unique trees: descriptor array {} bytes vs {} bytes of tree DTOs",
            plan.tree_hashes.len(),
            descriptor_bytes,
            wire_bytes
        );
        assert!(
            descriptor_bytes.saturating_mul(4) < wire_bytes,
            "whole-operation planning must not retain full TreeData payloads"
        );
    }

    #[test]
    fn trees_over_the_batch_cap_split_into_batches() {
        let blob_hash = hash_bytes(b"bounded object staging");
        let leaf = Tree::new(vec![TreeEntry {
            name: "file.txt".to_string(),
            kind: TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        let middle = Tree::new(vec![TreeEntry {
            name: "leaf".to_string(),
            kind: TreeEntryKind::Tree,
            hash: leaf.hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        let root = Tree::new(vec![TreeEntry {
            name: "middle".to_string(),
            kind: TreeEntryKind::Tree,
            hash: middle.hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        let tree_batches = split_staged_tree_batches_with_limits(
            vec![
                tree_to_wire(&leaf),
                tree_to_wire(&middle),
                tree_to_wire(&root),
            ],
            2,
            10,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(
            tree_batches.len(),
            2,
            "three trees exceed the test cap of two"
        );
    }
}
