use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use dialoguer::{Confirm, Input, Select};
use oak_core::{
    chunk_content, hash_bytes, Blob, Hash, MetadataKey, OakError, Result, LARGE_FILE_THRESHOLD,
};
use oak_core::{Repository, SqliteRepository};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::output;

/// Default number of concurrent chunk uploads.
///
/// Each upload to R2 runs on its own connection and tends to be throttled
/// per-connection (bandwidth-delay-product limited over a high-RTT path), so
/// aggregate throughput scales roughly linearly with concurrency until the
/// link's real ceiling — a single 1 MB-chunk stream leaves a fast uplink idle.
/// Override with `OAK_UPLOAD_CONCURRENCY` for very fast or very slow links.
const DEFAULT_CONCURRENT_TRANSFERS: usize = 32;
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

/// Resolve the chunk-upload concurrency, honoring `OAK_UPLOAD_CONCURRENCY`.
fn max_concurrent_transfers() -> usize {
    std::env::var("OAK_UPLOAD_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CONCURRENT_TRANSFERS)
}

/// HTTP client dedicated to chunk uploads.
///
/// The default `reqwest::Client` negotiates HTTP/2 with R2/Cloudflare, which
/// multiplexes *every* concurrent upload over a single TCP connection. h2's
/// flow-control window (~64 KB) then caps aggregate throughput at
/// `window / RTT` no matter how many tasks we spawn — the bug that pinned
/// uploads to ~1.5 MiB/s. Forcing HTTP/1.1 gives each concurrent upload its
/// own pooled connection (each with its own window), so throughput scales with
/// `OAK_UPLOAD_CONCURRENCY` up to the real link ceiling. We keep up to
/// `concurrency` connections warm for reuse across the batch.
fn upload_client(concurrency: usize) -> reqwest::Client {
    crate::http::ensure_crypto_provider();
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .http1_only()
        .pool_max_idle_per_host(concurrency)
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| crate::http::api_client())
}

/// Encode chunks into the length-framed body the server's `/chunks/batch`
/// endpoint expects: `[u32 BE hash_len][hash][u32 BE data_len][data]` per
/// entry. Binary framing avoids the ~5x inflation of JSON number arrays.
fn encode_chunk_batch(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let cap: usize = entries.iter().map(|(h, d)| 8 + h.len() + d.len()).sum();
    let mut buf = Vec::with_capacity(cap);
    for (hash, data) in entries {
        buf.extend_from_slice(&(hash.len() as u32).to_be_bytes());
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(data);
    }
    buf
}

// Wire protocol types live in `oak_core::protocol` (the single source of truth
// shared by the CLI, the hosted server, and `oak serve`). They're aliased back
// to the names this module has always used so the call sites below are
// unchanged. `BranchPushData`'s wire shape is identical to the old local
// `BranchData`, and `ChunkCheckServerResponse` to the old `ChunkCheckResponse`.
use oak_core::protocol::{
    BlobCheckRequest, BlobCheckResponse, BlobData, BlobProofChunk, BlobProofDescriptor,
    BlobProofFinalizeRequest, BlobProofMappingPage, BlobProofPagesRequest, BlobProofPagesResponse,
    BlobProofRequest, BlobProofResponse, BranchPushData as BranchData,
    ChunkCheckServerResponse as ChunkCheckResponse, ChunkRefData as ChunkRef, ChunkUploadInfo,
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
    let blob = repo.get_blob(expected_hash)?.ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "push admission phase rejected local blob {expected_hash}: metadata exists but bytes are missing; no remote state was mutated"
        ))
    })?;
    admit_local_blob(&blob, expected_hash)?;
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
    manifest_path_bytes: std::collections::HashMap<Hash, usize>,
    direct_tree_entries: usize,
    tree_metadata_bytes: usize,
    expanded_path_bytes: usize,
}

#[derive(Clone)]
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

fn validate_push_operation_caps(totals: PushOperationTotals) -> Result<()> {
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
            totals.resolved_manifest_entries,
            oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES,
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
        ("blobs", totals.blobs, oak_core::protocol::STAGED_MAX_BLOBS),
    ];
    for (dimension, actual, limit) in checks {
        if actual > limit {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected the whole operation: {dimension} total {actual} exceeds limit {limit}; reduce the outgoing history/content or split it into separate branches. No remote state was mutated"
            )));
        }
    }
    if totals.declared_blob_bytes > oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES {
        return Err(OakError::InvalidArgument(format!(
            "push admission phase rejected the whole operation: declared blob bytes total {} exceeds limit {}; reduce the outgoing content or split it into separate branches. No remote state was mutated",
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
    PhaseOneOrdinaryBootstrap,
    StagedReady,
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
    content_receipt_enforcement_required: bool,
}

#[derive(Debug, Clone, Copy)]
struct FirstPublicationState {
    repo_needs_creation: bool,
    phase_one_ordinary_allowed: bool,
}

async fn select_push_transport(
    client: &reqwest::Client,
    remote: &str,
    api_key: Option<&str>,
    totals: PushOperationTotals,
    allow_phase_one_ordinary_bootstrap: bool,
) -> Result<PushTransport> {
    if !select_staged_protocol(totals)? {
        return Ok(PushTransport::LegacyOrdinary);
    }
    match server_push_capability(client, remote, api_key)
        .await?
        .transport
    {
        ServerPushCapability::StagedReady => Ok(PushTransport::StagedReady),
        ServerPushCapability::PhaseOneOrdinaryBootstrap if allow_phase_one_ordinary_bootstrap => {
            Ok(PushTransport::PhaseOneOrdinaryBootstrap)
        }
        ServerPushCapability::Legacy | ServerPushCapability::PhaseOneOrdinaryBootstrap => {
            Ok(PushTransport::StagedRequiredUnavailable)
        }
    }
}

async fn select_push_transport_for_plan(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    api_key: Option<&str>,
    totals: PushOperationTotals,
    objects: &PlannedObjects,
    first_publication: FirstPublicationState,
) -> Result<PushTransport> {
    if !staged_protocol_required(totals) {
        return Ok(PushTransport::LegacyOrdinary);
    }
    let missing = if first_publication.repo_needs_creation {
        objects.blobs.iter().map(|blob| blob.hash.clone()).collect()
    } else {
        remote_missing_staged_blobs(
            client,
            remote,
            endpoint_path,
            api_key,
            &objects.blobs,
            false,
        )
        .await?
    };
    select_push_transport(
        client,
        remote,
        api_key,
        totals_for_remote_missing_blobs(totals, objects, &missing),
        first_publication.phase_one_ordinary_allowed,
    )
    .await
}

fn staged_required_unavailable_error() -> OakError {
    OakError::InvalidArgument(
        "this push exceeds the safe ordinary publication envelope and requires staged_v1, but the server has not advertised the complete staged capability set; retry after the server rollout is ready or split the history/content into smaller pushes; no remote state was mutated"
            .to_string(),
    )
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

fn validate_chunk_check_missing(
    requested: &[String],
    returned: Vec<ChunkUploadInfo>,
    context: &str,
) -> Result<Vec<ChunkUploadInfo>> {
    let expected: std::collections::HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    for chunk in &returned {
        if !expected.contains(chunk.hash.as_str()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned unrequested hash {}; no remote state was mutated",
                chunk.hash
            )));
        }
        if !seen.insert(chunk.hash.as_str()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned duplicate hash {}; no remote state was mutated",
                chunk.hash
            )));
        }
    }
    Ok(returned)
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

struct MappingPreflightWriter<'a> {
    chunk_hasher: blake3::Hasher,
    content_hasher: &'a mut blake3::Hasher,
    tempfile: Option<&'a mut std::fs::File>,
}

impl std::io::Write for MappingPreflightWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.chunk_hasher.update(bytes);
        self.content_hasher.update(bytes);
        if let Some(file) = self.tempfile.as_mut() {
            file.write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = self.tempfile.as_mut() {
            file.flush()
        } else {
            Ok(())
        }
    }
}

fn reserve_rechunk_file(file: &std::fs::File, bytes: u64) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let length = libc::off_t::try_from(bytes).map_err(|_| {
            OakError::InvalidArgument("rechunk reservation exceeds platform file size".to_string())
        })?;
        let mut reservation = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: length,
            fst_bytesalloc: 0,
        };
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut reservation) } == -1 {
            return Err(OakError::Io(std::io::Error::last_os_error()));
        }
        file.set_len(bytes)?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::fd::AsRawFd;
        let length = libc::off_t::try_from(bytes).map_err(|_| {
            OakError::InvalidArgument("rechunk reservation exceeds platform file size".to_string())
        })?;
        let status = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, length) };
        if status != 0 {
            return Err(OakError::Io(std::io::Error::from_raw_os_error(status)));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, bytes);
        return Err(OakError::InvalidArgument(
            "safe repository-local rechunking is not supported on this platform because Oak cannot reserve physical disk blocks and lock the repository atomically; push from macOS or Linux"
                .to_string(),
        ));
    }
    Ok(())
}

/// Repository-local, physically reserved rechunk workspace.
///
/// This path intentionally fails closed on non-Unix platforms: sparse
/// `set_len` and process-local locking cannot guarantee that the SQLite/WAL
/// write has capacity after preflight. Windows support requires native block
/// allocation and an inter-process repository lock before this can be enabled.
struct RechunkWorkspace {
    source: tempfile::NamedTempFile,
    persistence_reservation: tempfile::NamedTempFile,
    persistence_remaining: u64,
    lock: std::fs::File,
}

impl RechunkWorkspace {
    fn create(repo: &SqliteRepository, blob_size: u64) -> Result<Self> {
        Self::create_with_reserver(repo, blob_size, reserve_rechunk_file)
    }

    fn create_with_reserver(
        repo: &SqliteRepository,
        blob_size: u64,
        mut reserve: impl FnMut(&std::fs::File, u64) -> Result<()>,
    ) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (&repo, blob_size, &mut reserve);
            return Err(OakError::InvalidArgument(
                "safe repository-local rechunking is not supported on this platform because Oak cannot reserve physical disk blocks and lock the repository atomically; push from macOS or Linux"
                    .to_string(),
            ));
        }
        let db_path = repo.database_path()?;
        let directory = db_path.parent().ok_or_else(|| {
            OakError::InvalidArgument("repository database has no parent directory".to_string())
        })?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".oak-rechunk.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(OakError::Io(std::io::Error::last_os_error()));
            }
        }
        let source = tempfile::Builder::new()
            .prefix(".oak-rechunk-source-")
            .tempfile_in(directory)
            .map_err(OakError::Io)?;
        let persistence_reservation = tempfile::Builder::new()
            .prefix(".oak-rechunk-capacity-")
            .tempfile_in(directory)
            .map_err(OakError::Io)?;
        reserve(source.as_file(), blob_size)?;
        let database_and_wal = blob_size.checked_mul(2).ok_or_else(|| {
            OakError::InvalidArgument(
                "rechunk persistence reservation exceeds platform file size".to_string(),
            )
        })?;
        reserve(persistence_reservation.as_file(), database_and_wal)?;
        Ok(Self {
            source,
            persistence_reservation,
            persistence_remaining: database_and_wal,
            lock,
        })
    }

    fn source_file_mut(&mut self) -> &mut std::fs::File {
        self.source.as_file_mut()
    }

    fn transfer_remaining_capacity(&mut self) -> Result<()> {
        self.persistence_remaining = 0;
        self.persistence_reservation.as_file().set_len(0)?;
        Ok(())
    }

    fn persistence_is_durable(&self, repo: &SqliteRepository) -> Result<()> {
        repo.checkpoint_wal_truncate()?;
        Ok(())
    }
}

impl Drop for RechunkWorkspace {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn spill_inline_blobs_to_chunks(
    repo: &SqliteRepository,
    blobs: &mut [BlobData],
    sources: &mut Vec<PreparedChunk>,
) -> Result<()> {
    // Vec<u8> serializes as a JSON number array, so 15 MiB raw is already
    // close to the hosted request-body envelope. Persist the exact mapping
    // locally so the operation-wide preflight can later rematerialize it
    // without retaining the content in memory.
    const MAX_INLINE_BYTES: u64 = 15 * 1024 * 1024;
    let total_inline: u64 = blobs.iter().map(|blob| blob.content.len() as u64).sum();
    if total_inline <= MAX_INLINE_BYTES {
        return Ok(());
    }
    for blob in blobs {
        if !blob.chunks.is_empty() || blob.content.is_empty() {
            continue;
        }
        let data = std::mem::take(&mut blob.content);
        if data.len() as u64 > oak_core::protocol::MAPPING_PROOF_MAX_CHUNK_BYTES {
            for chunk in oak_core::stream_chunk_content(data.as_slice()) {
                let (info, bytes) = chunk?;
                repo.store_chunk(&info.hash, &bytes)?;
                let chunk_ref = ChunkRef {
                    hash: info.hash.to_string(),
                    offset: info.offset,
                    size: info.length,
                };
                sources.push(PreparedChunk::Stored(chunk_ref.clone()));
                blob.chunks.push(chunk_ref);
            }
        } else {
            let chunk_ref = ChunkRef {
                hash: blob.hash.clone(),
                offset: 0,
                size: u32::try_from(data.len()).map_err(|_| {
                    OakError::InvalidArgument(format!(
                        "inline blob {} exceeds the chunk length wire type; no remote state was mutated",
                        blob.hash
                    ))
                })?,
            };
            sources.push(PreparedChunk::Buffered(chunk_ref.clone(), data));
            blob.chunks = vec![chunk_ref];
        }
    }
    Ok(())
}

fn prepare_staged_mapping_proofs(
    repo: &SqliteRepository,
    blobs: &mut [BlobData],
    sources: &mut Vec<PreparedChunk>,
    max_blob_refs: usize,
) -> Result<Vec<PreparedMappingProof>> {
    let mut buffered = std::collections::HashMap::<String, Vec<u8>>::new();
    for source in sources.drain(..) {
        if let PreparedChunk::Buffered(chunk, bytes) = source {
            buffered.entry(chunk.hash).or_insert(bytes);
        }
    }

    let mut proofs = Vec::new();
    for blob in blobs.iter_mut().filter(|blob| !blob.chunks.is_empty()) {
        if blob.size > oak_core::protocol::MAPPING_PROOF_MAX_BLOB_BYTES {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected blob {}: declared size {} exceeds async_v1 per-blob limit {}; no remote state was mutated",
                blob.hash,
                blob.size,
                oak_core::protocol::MAPPING_PROOF_MAX_BLOB_BYTES
            )));
        }
        let needs_rechunk = blob.chunks.len() > max_blob_refs
            || blob
                .chunks
                .iter()
                .any(|chunk| chunk.size as u64 > oak_core::protocol::MAPPING_PROOF_MAX_CHUNK_BYTES);
        let mut tempfile = if needs_rechunk {
            let workspace = RechunkWorkspace::create(repo, blob.size).map_err(|error| {
                OakError::InvalidArgument(format!(
                    "push admission phase could not reserve repository-local temporary, SQLite, and WAL capacity for blob {}: {error}; no remote state was mutated",
                    blob.hash
                ))
            })?;
            Some(workspace)
        } else {
            None
        };
        let expected_hash = Hash::from_hex(&blob.hash)?;
        let mut expected_offset = 0u64;
        let mut content_hasher = blake3::Hasher::new();
        let mut original = Vec::with_capacity(blob.chunks.len());
        for chunk in &blob.chunks {
            if chunk.offset != expected_offset || chunk.size == 0 {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected blob {}: non-contiguous or empty chunk mapping; no remote state was mutated",
                    blob.hash
                )));
            }
            let chunk_hash = Hash::from_hex(&chunk.hash)?;
            let mut writer = MappingPreflightWriter {
                chunk_hasher: blake3::Hasher::new(),
                content_hasher: &mut content_hasher,
                tempfile: tempfile.as_mut().map(RechunkWorkspace::source_file_mut),
            };
            let copied = if let Some(bytes) = buffered.remove(&chunk.hash) {
                writer.write_all(&bytes)?;
                if !needs_rechunk {
                    repo.store_chunk(&chunk_hash, &bytes)?;
                }
                Some(bytes.len() as u64)
            } else {
                repo.copy_chunk_to_writer(&chunk_hash, &mut writer)?
            };
            if copied != Some(chunk.size as u64)
                || Hash(writer.chunk_hasher.finalize().to_hex().to_string()) != chunk_hash
            {
                return Err(OakError::InvalidArgument(format!(
                    "push admission phase rejected blob {}: local chunk {} fails size/hash verification; no remote state was mutated",
                    blob.hash, chunk.hash
                )));
            }
            original.push(oak_core::ChunkInfo {
                hash: chunk_hash,
                offset: chunk.offset,
                length: chunk.size,
            });
            expected_offset = expected_offset
                .checked_add(chunk.size as u64)
                .ok_or_else(|| OakError::InvalidArgument("chunk offsets overflow".to_string()))?;
        }
        let actual_hash = Hash(content_hasher.finalize().to_hex().to_string());
        if expected_offset != blob.size || actual_hash != expected_hash {
            return Err(OakError::InvalidArgument(format!(
                "push admission phase rejected blob {}: local mapping reconstructed {} bytes as {}; no remote state was mutated",
                blob.hash, expected_offset, actual_hash
            )));
        }

        let (final_mapping, mapping_already_persisted) = if let Some(mut workspace) = tempfile {
            workspace.source_file_mut().seek(SeekFrom::Start(0))?;
            repo.write_txn_begin()?;
            let persisted = (|| -> Result<Vec<oak_core::ChunkInfo>> {
                let mut mapping = Vec::new();
                let mut rechunked_hasher = blake3::Hasher::new();
                let RechunkWorkspace {
                    source,
                    persistence_reservation,
                    persistence_remaining,
                    ..
                } = &mut workspace;
                let source = source.as_file_mut().take(blob.size);
                for chunk in oak_core::stream_chunk_content(source) {
                    let (info, bytes) = chunk?;
                    if mapping.len() == max_blob_refs {
                        return Err(OakError::InvalidArgument(format!(
                            "push admission phase could not rechunk blob {} beneath async_v1's {}-reference limit; no remote state was mutated",
                            blob.hash, max_blob_refs
                        )));
                    }
                    rechunked_hasher.update(&bytes);
                    let release = (bytes.len() as u64)
                        .saturating_mul(2)
                        .min(*persistence_remaining);
                    *persistence_remaining -= release;
                    persistence_reservation
                        .as_file()
                        .set_len(*persistence_remaining)?;
                    repo.store_chunk(&info.hash, &bytes)?;
                    mapping.push(info);
                }
                let rechunked_hash = Hash(rechunked_hasher.finalize().to_hex().to_string());
                if rechunked_hash != expected_hash {
                    return Err(OakError::InvalidArgument(format!(
                        "push admission phase rechunked blob {} as {}; no remote state was mutated",
                        blob.hash, rechunked_hash
                    )));
                }
                workspace.transfer_remaining_capacity()?;
                repo.replace_blob_chunks(&expected_hash, &mapping)?;
                Ok(mapping)
            })();
            let mapping = match persisted {
                Ok(mapping) => mapping,
                Err(error) => {
                    repo.write_txn_rollback();
                    return Err(error);
                }
            };
            if let Err(error) = repo.write_txn_commit() {
                repo.write_txn_rollback();
                return Err(error);
            }
            workspace.persistence_is_durable(repo)?;
            (mapping, true)
        } else {
            (original, false)
        };
        if !mapping_already_persisted {
            repo.replace_blob_chunks(&expected_hash, &final_mapping)?;
        }
        blob.chunks = final_mapping
            .iter()
            .map(|chunk| ChunkRef {
                hash: chunk.hash.to_string(),
                offset: chunk.offset,
                size: chunk.length,
            })
            .collect();
        sources.extend(blob.chunks.iter().cloned().map(PreparedChunk::Stored));
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
        spill_inline_blobs_to_chunks(repo, &mut objects.blobs, &mut objects.chunks)?;
        prepare_staged_mapping_proofs(
            repo,
            &mut objects.blobs,
            &mut objects.chunks,
            oak_core::protocol::MAPPING_PROOF_MAX_BLOB_CHUNK_REFS,
        )?;
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
const STAGED_PUBLICATION_MAX_ATTEMPTS: usize = 3;
const STAGED_PUBLICATION_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const CHUNK_BUSY_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
const CHUNK_BUSY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Deserialize)]
struct ChunkObjectBusyResponse {
    error: String,
    retry_after_ms: u64,
}

struct ChunkBusyRetryState {
    overall_deadline: tokio::time::Instant,
    busy_wait_budget: std::time::Duration,
    busy_wait_reserved: tokio::sync::Mutex<std::time::Duration>,
}

impl ChunkBusyRetryState {
    fn new(overall_deadline: tokio::time::Instant, busy_wait_budget: std::time::Duration) -> Self {
        Self {
            overall_deadline,
            busy_wait_budget,
            busy_wait_reserved: tokio::sync::Mutex::new(std::time::Duration::ZERO),
        }
    }
}

fn chunk_request_remaining(
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> Result<std::time::Duration> {
    let remaining = retry_state
        .overall_deadline
        .saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(OakError::Server(format!(
            "{context} exceeded the size-aware push deadline; retry the push"
        )));
    }
    Ok(remaining)
}

async fn decode_chunk_response_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> Result<T> {
    let remaining = chunk_request_remaining(retry_state, context)?;
    tokio::time::timeout(remaining, response.json())
        .await
        .map_err(|_| {
            OakError::Server(format!(
                "{context} response body exceeded the size-aware push deadline"
            ))
        })?
        .map_err(|error| OakError::Http(error.to_string()))
}

async fn chunk_error_text_with_deadline(
    response: reqwest::Response,
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> String {
    let Ok(remaining) = chunk_request_remaining(retry_state, context) else {
        return format!("{context} response body exceeded the size-aware push deadline");
    };
    match tokio::time::timeout(remaining, crate::http::error_text(response)).await {
        Ok(detail) => detail,
        Err(_) => format!("{context} response body exceeded the size-aware push deadline"),
    }
}

async fn send_presigned_chunk_put(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    retry_state: &ChunkBusyRetryState,
) -> Result<reqwest::Response> {
    let remaining = chunk_request_remaining(retry_state, "presigned chunk upload")?;
    client
        .put(url)
        .header("content-type", "application/octet-stream")
        .body(body)
        .timeout(remaining)
        .send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))
}

async fn send_chunk_request_with_busy_retry(
    request: reqwest::RequestBuilder,
    retry_state: Arc<ChunkBusyRetryState>,
    context: &str,
) -> Result<reqwest::Response> {
    let template = request.try_clone().ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "{context} request body cannot be replayed safely; retry the push"
        ))
    })?;
    let mut backoff = std::time::Duration::from_millis(250);
    loop {
        let remaining = chunk_request_remaining(&retry_state, context)?;
        let attempt = template.try_clone().ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "{context} request body cannot be replayed safely; retry the push"
            ))
        })?;
        let response = crate::http::send_idempotent_with_retry_until(
            attempt,
            context,
            tokio::time::Instant::now() + remaining,
        )
        .await?;
        if response.status() != reqwest::StatusCode::CONFLICT {
            return Ok(response);
        }

        let retry_after_header = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or_default();
        let remaining_after_headers = retry_state
            .overall_deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining_after_headers.is_zero() {
            return Err(OakError::Server(format!(
                "{context} exhausted the bounded push deadline while reading a chunk maintenance conflict"
            )));
        }
        let body = tokio::time::timeout(remaining_after_headers, response.bytes())
            .await
            .map_err(|_| OakError::Server(format!("{context} conflict response body timed out")))?
            .map_err(|error| OakError::Http(error.to_string()))?;
        let busy: ChunkObjectBusyResponse = serde_json::from_slice(&body).map_err(|_| {
            OakError::Server(format!(
                "{context} returned 409 Conflict: {}",
                String::from_utf8_lossy(&body)
            ))
        })?;
        if busy.error != "chunk_object_busy" {
            return Err(OakError::Server(format!(
                "{context} returned 409 Conflict: {}",
                String::from_utf8_lossy(&body)
            )));
        }

        let advertised =
            retry_after_header.max(std::time::Duration::from_millis(busy.retry_after_ms));
        let wait = advertised.max(backoff.min(CHUNK_BUSY_MAX_BACKOFF));
        let remaining_before_sleep = retry_state
            .overall_deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let mut busy_reserved = retry_state.busy_wait_reserved.lock().await;
        let busy_remaining = retry_state.busy_wait_budget.saturating_sub(*busy_reserved);
        if wait > remaining_before_sleep || wait > busy_remaining {
            return Err(OakError::Server(format!(
                "{context} remained fenced by chunk object maintenance past its cumulative busy-wait budget; retry the push"
            )));
        }
        *busy_reserved += wait;
        drop(busy_reserved);
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(CHUNK_BUSY_MAX_BACKOFF);
    }
}

fn mapping_proof_overall_timeout(declared_bytes: u64) -> std::time::Duration {
    // One MiB/s is deliberately conservative enough for a congested agent or
    // local Serve process, while the 23-hour ceiling leaves an hour to abort or
    // finalize before the staged session's 24-hour expiry.
    let transfer_seconds = declared_bytes.div_ceil(1024 * 1024);
    std::time::Duration::from_secs((15 * 60 + transfer_seconds).clamp(60 * 60, 23 * 60 * 60))
}

fn staged_publication_overall_timeout(request_bytes: usize) -> std::time::Duration {
    let transfer_seconds = (request_bytes as u64)
        .div_ceil(1024 * 1024)
        .saturating_mul(4);
    std::time::Duration::from_secs((30 + transfer_seconds).clamp(30, 30 * 60))
}

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
    deadline: tokio::time::Instant,
    request_timeout: std::time::Duration,
) -> Result<PushResponse> {
    let template = request.try_clone().ok_or_else(|| {
        OakError::InvalidArgument(
            "staged publication request cannot be replayed safely; no head was advanced"
                .to_string(),
        )
    })?;
    let mut last_error = "staged publication response was unavailable".to_string();
    for attempt_index in 0..STAGED_PUBLICATION_MAX_ATTEMPTS {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let attempt = template.try_clone().ok_or_else(|| {
            OakError::InvalidArgument(
                "staged publication request cannot be replayed safely; no head was advanced"
                    .to_string(),
            )
        })?;
        let response = match tokio::time::timeout(
            request_timeout.min(remaining),
            attempt.timeout(request_timeout.min(remaining)).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                last_error = error.to_string();
                continue;
            }
            Err(_) => {
                last_error = "response headers timed out".to_string();
                continue;
            }
        };
        let status = response.status();
        if status.is_success() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match read_staged_publication_body(response, request_timeout.min(remaining)).await {
                Ok(body) => match serde_json::from_slice(&body) {
                    Ok(decoded) => return Ok(decoded),
                    Err(error) => last_error = format!("response JSON failed: {error}"),
                },
                Err(error) => last_error = error.to_string(),
            }
            if attempt_index + 1 < STAGED_PUBLICATION_MAX_ATTEMPTS {
                continue;
            }
            break;
        }
        if matches!(status.as_u16(), 404 | 405) {
            return Err(OakError::InvalidArgument(
                "server replica does not support the required staged-v1 push endpoint; upgrade or retry after deployment converges. No head was advanced"
                    .to_string(),
            ));
        }
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
        {
            last_error = format!("HTTP {status}");
            continue;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let body = read_staged_publication_body(response, request_timeout.min(remaining)).await?;
        let detail = String::from_utf8_lossy(&body);
        return Err(OakError::Server(if detail.trim().is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {}", detail.trim())
        }));
    }
    Err(OakError::Server(format!(
        "staged publication did not return a complete bounded response after {STAGED_PUBLICATION_MAX_ATTEMPTS} exact idempotent attempt(s): {last_error}; retry the push"
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

fn materialize_planned_blob_batch(
    repo: &SqliteRepository,
    planned: Vec<PlannedBlob>,
) -> Result<PreplannedObjects> {
    let mut blobs = Vec::with_capacity(planned.len());
    let mut chunks = Vec::new();
    for descriptor in planned {
        if let Some(mut mapping) = repo.get_blob_chunks(&descriptor.hash)? {
            if !mapping.is_empty() {
                mapping.sort_by_key(|chunk| chunk.offset);
                let refs: Vec<ChunkRef> = mapping
                    .iter()
                    .map(|chunk| ChunkRef {
                        hash: chunk.hash.to_string(),
                        offset: chunk.offset,
                        size: chunk.length,
                    })
                    .collect();
                chunks.extend(mapping.into_iter().map(|chunk| {
                    PreparedChunk::Stored(ChunkRef {
                        hash: chunk.hash.to_string(),
                        offset: chunk.offset,
                        size: chunk.length,
                    })
                }));
                blobs.push(BlobData {
                    hash: descriptor.hash.to_string(),
                    content: Vec::new(),
                    size: descriptor.size,
                    chunks: refs,
                    mapping_proof_token: None,
                });
                continue;
            }
        }
        let blob = repo.get_blob(&descriptor.hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "push admission phase lost staged blob {}; no remote state was mutated",
                descriptor.hash
            ))
        })?;
        admit_local_blob(&blob, &descriptor.hash)?;
        blobs.push(BlobData {
            hash: descriptor.hash.to_string(),
            content: blob.content,
            size: descriptor.size,
            chunks: Vec::new(),
            mapping_proof_token: None,
        });
    }
    Ok(PreplannedObjects {
        trees: Vec::new(),
        blobs,
        chunks,
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
    expected_head: Option<Hash>,
    target_head: Hash,
    commits: Vec<oak_core::Commit>,
    objects: PlannedObjects,
    progress: Option<&indicatif::ProgressBar>,
) -> Result<()> {
    let client = crate::http::api_client();
    let PlannedObjects {
        tree_hashes,
        blobs,
        manifest_entries,
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
    let stage_id = uuid::Uuid::new_v4().simple().to_string();
    let abort_expected_head = expected_head.clone();
    let operation = async {
        for batch in blob_batches {
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
                    stage_id: Some(stage_id.clone()),
                    expected_head: expected_head.clone(),
                    commits: Vec::new(),
                    staged_protocol: true,
                    finalize: false,
                    target_head: None,
                    objects: Some(objects),
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
                    stage_id: Some(stage_id.clone()),
                    expected_head: expected_head.clone(),
                    commits: Vec::new(),
                    staged_protocol: true,
                    finalize: false,
                    target_head: None,
                    objects: Some(objects),
                }),
                None,
                false,
            ))
            .await?;
        }
        for batch in split_staged_commit_batches(commits, &manifest_entries, &manifest_path_bytes)?
        {
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
                }),
                None,
                false,
            ))
            .await?;
            if let Some(progress) = progress {
                progress.inc(batch_len as u64);
            }
        }
        Box::pin(push_async_with_endpoint(
            repo,
            work_tree,
            remote,
            endpoint_path,
            Some(branch_name),
            force,
            api_key,
            Some(PreplannedBatch {
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
            }),
            None,
            false,
        ))
        .await
    };
    tokio::pin!(operation);
    let result = tokio::select! {
        result = &mut operation => result,
        signal = tokio::signal::ctrl_c() => {
            let detail = signal.map_or_else(
                |error| format!("push cancellation monitor failed: {error}"),
                |_| "push interrupted before staged finalization".to_string(),
            );
            Err(OakError::InvalidArgument(detail))
        }
    };
    if result.is_err() {
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
    for batch in preflight_staged_blob_batches(repo, blobs)? {
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
                stage_id: None,
                expected_head: None,
                commits: Vec::new(),
                staged_protocol: false,
                finalize: false,
                target_head: None,
                objects: Some(objects),
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
                stage_id: None,
                expected_head: None,
                commits: Vec::new(),
                staged_protocol: false,
                finalize: false,
                target_head: None,
                objects: Some(objects),
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

fn collect_planned_objects(
    repo: &SqliteRepository,
    commits: &[&oak_core::Commit],
) -> Result<PlannedObjects> {
    let mut tree_state: std::collections::HashMap<Hash, u8> = std::collections::HashMap::new();
    let mut seen_blobs = std::collections::HashSet::new();
    let mut missing_blobs = std::collections::HashSet::new();
    let mut tree_hashes = Vec::new();
    let mut blobs = Vec::new();
    let mut manifest_entries = std::collections::HashMap::new();
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
            for entry in &tree.entries {
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
            manifest_entries.insert(hash.clone(), resolved_entries);
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
        stack.push((hash, true));
        for entry in tree.entries.into_iter().rev() {
            if entry.kind == oak_core::TreeEntryKind::Tree {
                stack.push((entry.hash, false));
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
        manifest_path_bytes,
        direct_tree_entries,
        tree_metadata_bytes,
        expanded_path_bytes,
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
}

#[derive(Debug)]
struct EndpointPushOutcome {
    published: bool,
    current_branch_pushed_head: Option<Hash>,
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
    let remote = resolve_push_remote(path, remote_url)?;
    run_resolved_with_outcome(path, &remote.url, remote.persist, force, repo_spec).await?;
    Ok(())
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
    let remote = resolve_push_remote(path, remote_url)?;
    // Resolve every local preflight before capture begins. If resolution
    // fails, main can still emit the single JSON error envelope; starting a
    // capture first would swallow that envelope when `?` returns early.
    output::begin_capture();
    let result =
        run_resolved_with_outcome(path, &remote.url, remote.persist, force, repo_spec).await;
    let _narration = output::end_capture();
    let outcome = result?;
    let review_url = super::branch_web_url(&outcome.remote, &outcome.repo, &outcome.branch);

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
        recommended_next_commands: vec![
            "oak ci status --json".to_string(),
            format!(
                "oak branch review {} --remote --merge-preview --json",
                outcome.branch
            ),
        ],
    })
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

    if persist_remote {
        repo.set_metadata(MetadataKey::RemoteUrl, remote_url)?;
    }

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    // Get API key from env var, per-repo metadata, or global credentials
    let api_key = super::credentials::effective_token(
        remote_url,
        repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
    );

    let owner_meta = repo.get_metadata(MetadataKey::RepoOwner)?;
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
        link_remote_identity(&repo, remote_url, spec)?
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
    let resp = with_auth(
        client
            .post(format!("{remote}/api/{owner}/{repo_name}/push"))
            .header("x-oak-user", super::commit::get_author())
            .json(&push_req),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    // The push endpoint signals rejection as HTTP 200 + `success: false`
    // (e.g. the conflict check), so an OK status alone doesn't mean the
    // branch row landed.
    let body: PushResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !body.success {
        return Err(OakError::Server(body.message));
    }
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
    /// Bytes already resident because a legacy inline blob had to be split.
    Buffered(ChunkRef, Vec<u8>),
    /// Content-addressed local storage location. Bytes are re-read and
    /// re-verified only when this bounded upload page is processed.
    Stored(ChunkRef),
}

struct PreplannedBatch {
    stage_id: Option<String>,
    expected_head: Option<Hash>,
    commits: Vec<oak_core::Commit>,
    staged_protocol: bool,
    finalize: bool,
    target_head: Option<Hash>,
    objects: Option<PreplannedObjects>,
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

    // Get remote head — and, for a branch-scoped push, the branch head in
    // the same network window.
    // (Progress narration in this flow goes through push_info/push_success:
    // piped consumers get a single result line at the end instead.) The two GETs are independent reads and each
    // costs a full server round trip (~100-250ms against oak.space), so
    // sequencing them doubled the fixed latency of every push. A 404 from
    // the branch GET (repo or branch not on the server yet) means "no
    // remote head", same as before.
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
    // Tracks whether we auto-created the repo on this push. Used after
    // the push succeeds to prompt for a merge — a fresh repo with only a
    // feature-branch push looks empty in the web UI until `main` has a
    // commit, which reads like the push silently failed.
    let mut created_repo = false;
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
            let on_main_child = name != "main"
                && repo
                    .get_branch(name)?
                    .and_then(|branch| branch.parent_branch)
                    .is_some_and(|parent| parent == "main");
            if on_main_child && repo.get_branch_head("main")?.is_some() {
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
                    let transport = select_push_transport_for_plan(
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
                        },
                    )
                    .await?;
                    if transport == PushTransport::StagedRequiredUnavailable {
                        return Err(staged_required_unavailable_error());
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
                        )
                        .await?;
                    } else if transport == PushTransport::StagedReady {
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
                            expected_head,
                            main_tip,
                            main_commits,
                            objects,
                            Some(&pb),
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
                                    stage_id: None,
                                    expected_head: expected_head.clone(),
                                    commits: batch,
                                    staged_protocol: false,
                                    finalize: false,
                                    target_head: None,
                                    objects: None,
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
            let diverged = match branch_name {
                Some(name) => match repo.get_branch_head(name)? {
                    Some(lh) => lh != *rh && !super::sync::local_history_contains(repo, &lh, rh)?,
                    None => false,
                },
                None => repo.get_head()?.is_some() && !repo.has_commit(rh)?,
            };
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
    let preplanned_control = preplanned_batch.as_ref().map(|batch| {
        (
            batch.stage_id.clone(),
            batch.staged_protocol,
            batch.finalize,
            batch.expected_head.clone(),
            batch.target_head.clone(),
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
        let allowed_external: std::collections::HashSet<String> = [
            repo_head.as_ref(),
            remote_head.as_ref(),
            bootstrap_head.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(ToString::to_string)
        .collect();
        let required_boundary = (!force).then_some(remote_head.as_ref()).flatten();
        plan_target_with_remote_proofs(
            repo,
            &client,
            remote,
            endpoint_path,
            api_key,
            name,
            repo.get_branch_head(name)?.as_ref(),
            required_boundary,
            allowed_external,
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
    if preplanned_control.is_none() && !commits.is_empty() {
        if let Some(name) = branch_name {
            let refs: Vec<&oak_core::Commit> = commits.iter().collect();
            let objects = collect_planned_objects(repo, &refs)?;
            let branch = repo
                .get_branch(name)?
                .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
            let totals = push_operation_totals(&refs, &objects, &[branch_to_push_data(&branch)]);
            let phase_one_bootstrap = repo_head.is_none()
                && remote_head.is_none()
                && outgoing_operation_is_self_contained(&commits);
            let transport = select_push_transport_for_plan(
                &client,
                remote,
                endpoint_path,
                api_key,
                totals,
                &objects,
                FirstPublicationState {
                    repo_needs_creation,
                    phase_one_ordinary_allowed: phase_one_bootstrap,
                },
            )
            .await?;
            if transport == PushTransport::StagedRequiredUnavailable {
                return Err(staged_required_unavailable_error());
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
                )
                .await?;
                if emit_result {
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
                return Ok(EndpointPushOutcome {
                    published: true,
                    current_branch_pushed_head: target,
                });
            }
            if transport == PushTransport::StagedReady {
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
                    remote_head.clone(),
                    target.clone(),
                    commits,
                    objects,
                    None,
                )
                .await?;
                if emit_result {
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
                return Ok(EndpointPushOutcome {
                    published: true,
                    current_branch_pushed_head: Some(target),
                });
            }
        }
    }

    if commits.is_empty()
        && !has_preplanned_objects
        && preplanned_control
            .as_ref()
            .is_none_or(|(_, _, _, _, target)| target.is_none())
    {
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
        return Ok(EndpointPushOutcome {
            published: false,
            current_branch_pushed_head: remote_head,
        });
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
                    oak_core::TreeEntryKind::Blob if seen_blobs.insert(entry.hash.clone()) => {
                        if let Some(blob) = repo.get_blob(&entry.hash)? {
                            admit_local_blob(&blob, &entry.hash)?;
                            if blob.size >= LARGE_FILE_THRESHOLD {
                                let chunked = chunk_content(&blob.content);
                                let chunk_refs: Vec<ChunkRef> = chunked
                                    .iter()
                                    .map(|(info, _)| ChunkRef {
                                        hash: info.hash.to_string(),
                                        offset: info.offset,
                                        size: info.length,
                                    })
                                    .collect();

                                for (info, data) in chunked {
                                    all_chunk_data.push(PreparedChunk::Buffered(
                                        ChunkRef {
                                            hash: info.hash.to_string(),
                                            offset: info.offset,
                                            size: info.length,
                                        },
                                        data,
                                    ));
                                }

                                blobs.push(BlobData {
                                    hash: blob.hash.to_string(),
                                    content: Vec::new(),
                                    size: blob.size,
                                    chunks: chunk_refs,
                                    mapping_proof_token: None,
                                });
                            } else {
                                blobs.push(BlobData {
                                    hash: blob.hash.to_string(),
                                    content: blob.content,
                                    size: blob.size,
                                    chunks: Vec::new(),
                                    mapping_proof_token: None,
                                });
                            }
                        } else {
                            missing_local_blobs.insert(entry.hash);
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
        .is_some_and(|(_, staged, _, _, _)| *staged);
    let intended_published_head = if staged {
        preplanned_control
            .as_ref()
            .and_then(|(_, _, finalize, _, target)| finalize.then(|| target.clone()).flatten())
    } else {
        commit_data.last().map(|commit| Hash(commit.hash.clone()))
    };

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
                    let blob = repo.get_blob(&hash)?.ok_or_else(|| {
                        OakError::InvalidArgument(format!(
                            "push admission phase restored no bytes for blob {hash}; no remote state was mutated"
                        ))
                    })?;
                    admit_local_blob(&blob, &hash)?;
                    if blob.size >= LARGE_FILE_THRESHOLD {
                        let chunked = chunk_content(&blob.content);
                        let chunks: Vec<ChunkRef> = chunked
                            .iter()
                            .map(|(info, _)| ChunkRef {
                                hash: info.hash.to_string(),
                                offset: info.offset,
                                size: info.length,
                            })
                            .collect();
                        all_chunk_data.extend(chunked.into_iter().map(|(info, data)| {
                            PreparedChunk::Buffered(
                                ChunkRef {
                                    hash: info.hash.to_string(),
                                    offset: info.offset,
                                    size: info.length,
                                },
                                data,
                            )
                        }));
                        blobs.push(BlobData {
                            hash: blob.hash.to_string(),
                            content: Vec::new(),
                            size: blob.size,
                            chunks,
                            mapping_proof_token: None,
                        });
                    } else {
                        blobs.push(BlobData {
                            hash: blob.hash.to_string(),
                            content: blob.content,
                            size: blob.size,
                            chunks: Vec::new(),
                            mapping_proof_token: None,
                        });
                    }
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
    let skip_blob_check =
        !blobs.is_empty() && all_chunk_data.is_empty() && inline_payload < BLOB_CHECK_MIN_BYTES;
    if skip_blob_check {
        output::vlog(&format!(
            "push: skipping blobs/check ({} blob(s), {inline_payload} inline bytes < {BLOB_CHECK_MIN_BYTES})",
            blobs.len()
        ));
    }
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

    spill_inline_blobs_to_chunks(repo, &mut blobs, &mut all_chunk_data)?;

    let total_inline_bytes: u64 = blobs.iter().map(|b| b.content.len() as u64).sum();
    output::vlog(&format!(
        "push: collected {} blob(s) ({} inline bytes), {} tree(s), {} chunk(s) in {:.3}s",
        blobs.len(),
        total_inline_bytes,
        trees.len(),
        all_chunk_data.len(),
        t_collect.elapsed().as_secs_f64()
    ));

    // Persist/verify/rechunk every missing mapping locally before the first
    // remote mutation. Then create its exact inactive proof generation and
    // upload bounded metadata pages before any chunk upload.
    let mut uploaded_mapping_proof = None;
    let mapping_proofs = if staged {
        prepare_staged_mapping_proofs(
            repo,
            &mut blobs,
            &mut all_chunk_data,
            oak_core::protocol::MAPPING_PROOF_MAX_BLOB_CHUNK_REFS,
        )?
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
        let deadline = tokio::time::Instant::now() + mapping_proof_overall_timeout(total_bytes);
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

    // Upload chunks for large blobs in bounded pages. Planned staged pushes
    // carry only content-addressed locations, so even a multi-gigabyte blob
    // retains at most one page of chunk bytes while checking/uploading.
    // FastCDC chunks top out at 8 MiB. A 32-object page therefore also stays
    // within the hosted 256 MiB aggregate confirmation cap.
    const CHUNK_CHECK_PAGE: usize = 32;
    let chunk_declared_bytes = blobs
        .iter()
        .filter(|blob| !blob.chunks.is_empty())
        .fold(0u64, |total, blob| total.saturating_add(blob.size));
    let chunk_overall_deadline = uploaded_mapping_proof
        .as_ref()
        .map(|(_, deadline)| *deadline)
        .unwrap_or_else(|| {
            tokio::time::Instant::now() + mapping_proof_overall_timeout(chunk_declared_bytes)
        });
    let chunk_retry_state = Arc::new(ChunkBusyRetryState::new(
        chunk_overall_deadline,
        CHUNK_BUSY_RETRY_BUDGET,
    ));
    let mut pending_chunks = all_chunk_data.into_iter();
    loop {
        let mut unique_chunks: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        while unique_chunks.len() < CHUNK_CHECK_PAGE {
            let Some(source) = pending_chunks.next() else {
                break;
            };
            let (chunk_ref, data) = match source {
                PreparedChunk::Buffered(chunk_ref, data) => (chunk_ref, data),
                PreparedChunk::Stored(chunk_ref) => {
                    let hash = Hash::from_hex(&chunk_ref.hash)?;
                    let data = repo.get_chunk(&hash)?.ok_or_else(|| {
                        OakError::InvalidArgument(format!(
                            "push admission phase lost staged chunk {hash}; no branch state was mutated"
                        ))
                    })?;
                    if data.len() != chunk_ref.size as usize || hash_bytes(&data) != hash {
                        return Err(OakError::InvalidArgument(format!(
                            "push admission phase rejected staged chunk {hash}; no branch state was mutated"
                        )));
                    }
                    (chunk_ref, data)
                }
            };
            unique_chunks.entry(chunk_ref.hash).or_insert(data);
        }
        if unique_chunks.is_empty() {
            break;
        }

        let chunk_hashes: Vec<String> = unique_chunks.keys().cloned().collect();
        // Sizes parallel to chunk_hashes — sent so the server can enforce the
        // organization storage quota before issuing presigned upload URLs.
        let chunk_sizes: Vec<u64> = chunk_hashes
            .iter()
            .map(|h| unique_chunks.get(h).map(|d| d.len() as u64).unwrap_or(0))
            .collect();

        push_info(&format!(
            "Checking {} chunk(s) on server...",
            chunk_hashes.len()
        ));

        // Ask server which chunks are missing (returns presigned upload URLs if R2 is configured)
        output::vlog(&format!(
            "push: POST {}/api/{}/chunks/check ({} hashes)",
            remote,
            endpoint_path,
            chunk_hashes.len()
        ));
        let t_check = std::time::Instant::now();
        let check_request = with_auth(
            client
                .post(format!("{remote}/api/{endpoint_path}/chunks/check"))
                .json(&serde_json::json!({
                    "hashes": &chunk_hashes,
                    "sizes": chunk_sizes,
                    "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL,
                })),
            api_key,
        );
        let check_resp = send_chunk_request_with_busy_retry(
            check_request,
            chunk_retry_state.clone(),
            "chunk check",
        )
        .await?;
        output::vlog(&format!(
            "push: chunks/check returned {} in {:.3}s",
            check_resp.status(),
            t_check.elapsed().as_secs_f64()
        ));

        if !check_resp.status().is_success() {
            let detail =
                chunk_error_text_with_deadline(check_resp, &chunk_retry_state, "chunk check").await;
            return Err(OakError::Server(format!(
                "Failed to check chunks: {}",
                detail
            )));
        }

        let check_result: ChunkCheckResponse =
            decode_chunk_response_json(check_resp, &chunk_retry_state, "chunk check").await?;
        let missing = validate_chunk_check_missing(
            &chunk_hashes,
            check_result.missing,
            "chunk check response",
        )?;

        if !missing.is_empty() {
            let total = missing.len();
            let total_bytes: u64 = missing
                .iter()
                .filter_map(|info| unique_chunks.get(&info.hash).map(|d| d.len() as u64))
                .sum();

            let pb = indicatif::ProgressBar::new(total_bytes);
            pb.set_style(
                indicatif::ProgressStyle::default_bar()
                    .template(
                        "  Uploading [{bar:30.green/dim}] {bytes}/{total_bytes} ({bytes_per_sec})",
                    )
                    .unwrap()
                    .progress_chars("━╸─"),
            );

            // Partition missing chunks by size. Tiny objects (below the FastCDC
            // floor) are the killer: a push that spills thousands of small blobs
            // into the chunk pipeline would do one presigned PUT each, paying a
            // fixed ~0.5s round-trip per object regardless of payload. Those ship
            // in bulk to the server's /chunks/batch endpoint instead — a handful
            // of large requests, with R2 writes fanned out server-side. Genuinely
            // large CDC chunks keep the presigned-direct-to-R2 path, where the
            // per-request RTT amortizes over megabytes.
            const SMALL_CHUNK_THRESHOLD: usize = 256 * 1024; // == FastCDC MIN_CHUNK_SIZE
            const BATCH_MAX_BYTES: usize = 12 * 1024 * 1024; // per bulk request, under CF's 100MB body cap
            const BATCH_CONCURRENCY: usize = 6;

            let mut small: Vec<(String, Vec<u8>)> = Vec::new();
            let mut large: Vec<(String, Option<String>, Vec<u8>)> = Vec::new();
            for info in missing {
                if let Some(data) = unique_chunks.remove(&info.hash) {
                    if data.len() < SMALL_CHUNK_THRESHOLD {
                        small.push((info.hash, data));
                    } else {
                        large.push((info.hash, info.upload_url, data));
                    }
                }
            }

            let concurrency = max_concurrent_transfers();
            output::vlog(&format!(
                "push: uploading {} large chunk(s) (presigned, concurrency={concurrency}) + {} small chunk(s) (batched, concurrency={BATCH_CONCURRENCY})",
                large.len(),
                small.len(),
            ));
            let t_upload = std::time::Instant::now();

            // Shared HTTP/1.1 client: each concurrent upload gets its own
            // connection instead of being multiplexed (and flow-control
            // throttled) onto one shared HTTP/2 connection — see `upload_client`.
            let upload_client = upload_client(concurrency.max(BATCH_CONCURRENCY));
            let remote_owned = remote.to_string();
            let endpoint_owned = endpoint_path.to_string();
            let api_key_owned = api_key.map(|s| s.to_string());

            // --- Large chunks: presigned-direct-to-R2 (server-proxied fallback) ---
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let mut join_set: JoinSet<Result<Option<(String, u64)>>> = JoinSet::new();
            for (hash, upload_url, data) in large {
                let client = upload_client.clone();
                let pb = pb.clone();
                let sem = semaphore.clone();
                let remote = remote_owned.clone();
                let endpoint = endpoint_owned.clone();
                let api_key = api_key_owned.clone();
                let retry_state = chunk_retry_state.clone();

                join_set.spawn(async move {
                    let _permit = sem
                        .acquire_owned()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    let data_len = data.len() as u64;
                    let t_chunk = std::time::Instant::now();

                    if let Some(url) = upload_url {
                        // Upload directly to R2 via presigned URL. The server
                        // never sees these bytes (so can't compress them for
                        // us); compress here. Download paths detect-and-decode.
                        let body = oak_core::chunk_encode(&data);
                        let resp =
                            send_presigned_chunk_put(&client, &url, body, &retry_state).await?;
                        if !resp.status().is_success() {
                            let detail = chunk_error_text_with_deadline(
                                resp,
                                &retry_state,
                                "presigned chunk upload",
                            )
                            .await;
                            return Err(OakError::Server(format!(
                                "Failed to upload chunk to R2: {}",
                                detail
                            )));
                        }
                        let dt = t_chunk.elapsed().as_secs_f64();
                        output::vlog(&format!(
                            "push: chunk {} ({} B) -> R2 in {:.3}s ({:.0} KiB/s)",
                            &hash[..hash.len().min(8)],
                            data_len,
                            dt,
                            (data_len as f64 / 1024.0) / dt.max(0.001),
                        ));
                        pb.inc(data_len);
                        Ok(Some((hash, data_len)))
                    } else {
                        // No R2 — upload through the server
                        let mut builder = client
                            .put(format!("{remote}/api/{endpoint}/chunks/{hash}"))
                            .header("content-type", "application/octet-stream")
                            .body(data);
                        if let Some(ref key) = api_key {
                            builder = builder.header("authorization", format!("Bearer {key}"));
                        }
                        let resp = send_chunk_request_with_busy_retry(
                            builder,
                            retry_state.clone(),
                            "server chunk upload",
                        )
                        .await?;
                        if !resp.status().is_success() {
                            let detail = chunk_error_text_with_deadline(
                                resp,
                                &retry_state,
                                "server chunk upload",
                            )
                            .await;
                            return Err(OakError::Server(format!(
                                "Failed to upload chunk: {}",
                                detail
                            )));
                        }
                        let dt = t_chunk.elapsed().as_secs_f64();
                        output::vlog(&format!(
                            "push: chunk {} ({} B) -> server in {:.3}s ({:.0} KiB/s)",
                            &hash[..hash.len().min(8)],
                            data_len,
                            dt,
                            (data_len as f64 / 1024.0) / dt.max(0.001),
                        ));
                        pb.inc(data_len);
                        Ok(None)
                    }
                });
            }

            // Collect large-chunk results, then check for errors
            let mut raw_results = Vec::new();
            while let Some(result) = join_set.join_next().await {
                raw_results.push(result);
            }

            let mut r2_uploaded: Vec<(String, u64)> = Vec::new();
            for result in raw_results {
                let inner =
                    result.map_err(|e| OakError::Http(format!("Upload task panicked: {e}")))?;
                if let Some(r2_info) = inner? {
                    r2_uploaded.push(r2_info);
                }
            }

            // --- Small chunks: bulk batches through the server ---
            if !small.is_empty() {
                // Pack into batches under BATCH_MAX_BYTES (8 bytes of framing
                // overhead per entry: two u32 length prefixes).
                let mut batches: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
                let mut cur: Vec<(String, Vec<u8>)> = Vec::new();
                let mut cur_bytes = 0usize;
                for (hash, data) in small {
                    let entry_bytes = 8 + hash.len() + data.len();
                    if (cur_bytes + entry_bytes > BATCH_MAX_BYTES || cur.len() == 256)
                        && !cur.is_empty()
                    {
                        batches.push(std::mem::take(&mut cur));
                        cur_bytes = 0;
                    }
                    cur_bytes += entry_bytes;
                    cur.push((hash, data));
                }
                if !cur.is_empty() {
                    batches.push(cur);
                }

                output::vlog(&format!("push: {} small-chunk batch(es)", batches.len()));
                let batch_sem = Arc::new(Semaphore::new(BATCH_CONCURRENCY));
                let mut batch_set: JoinSet<Result<()>> = JoinSet::new();
                for batch in batches {
                    let client = upload_client.clone();
                    let pb = pb.clone();
                    let sem = batch_sem.clone();
                    let remote = remote_owned.clone();
                    let endpoint = endpoint_owned.clone();
                    let api_key = api_key_owned.clone();
                    let retry_state = chunk_retry_state.clone();

                    batch_set.spawn(async move {
                        let _permit = sem
                            .acquire_owned()
                            .await
                            .map_err(|e| OakError::Http(e.to_string()))?;
                        let batch_bytes: u64 = batch.iter().map(|(_, d)| d.len() as u64).sum();
                        let body = encode_chunk_batch(&batch);
                        let t = std::time::Instant::now();
                        let mut builder = client
                            .post(format!("{remote}/api/{endpoint}/chunks/batch"))
                            .header("content-type", "application/octet-stream")
                            .body(body);
                        if let Some(ref key) = api_key {
                            builder = builder.header("authorization", format!("Bearer {key}"));
                        }
                        let resp = send_chunk_request_with_busy_retry(
                            builder,
                            retry_state.clone(),
                            "chunk batch upload",
                        )
                        .await?;
                        if !resp.status().is_success() {
                            let detail = chunk_error_text_with_deadline(
                                resp,
                                &retry_state,
                                "chunk batch upload",
                            )
                            .await;
                            return Err(OakError::Server(format!(
                                "Failed to upload chunk batch: {}",
                                detail
                            )));
                        }
                        output::vlog(&format!(
                            "push: batch of {} chunk(s) ({} B) -> server in {:.3}s",
                            batch.len(),
                            batch_bytes,
                            t.elapsed().as_secs_f64(),
                        ));
                        pb.inc(batch_bytes);
                        Ok(())
                    });
                }
                while let Some(res) = batch_set.join_next().await {
                    res.map_err(|e| OakError::Http(format!("Batch task panicked: {e}")))??;
                }
            }

            pb.finish_and_clear();

            output::vlog(&format!(
                "push: chunk uploads done in {:.3}s",
                t_upload.elapsed().as_secs_f64()
            ));
            push_success(&format!("Uploaded {total} chunk(s)"));

            // Confirm presigned (large) R2 uploads so the server records their
            // metadata. Batched chunks are already recorded server-side by
            // /chunks/batch, so they're not in `r2_uploaded`.
            if !r2_uploaded.is_empty() {
                let entries: Vec<serde_json::Value> = r2_uploaded
                    .iter()
                    .map(|(hash, size)| serde_json::json!({ "hash": hash, "size": size }))
                    .collect();
                let confirm_request = with_auth(
                    client
                        .post(format!("{remote}/api/{endpoint_path}/chunks/uploaded"))
                        .json(&serde_json::json!({ "hashes": entries })),
                    api_key,
                );
                let confirm_resp = send_chunk_request_with_busy_retry(
                    confirm_request,
                    chunk_retry_state.clone(),
                    "chunk upload confirmation",
                )
                .await?;
                if !confirm_resp.status().is_success() {
                    let detail = chunk_error_text_with_deadline(
                        confirm_resp,
                        &chunk_retry_state,
                        "chunk upload confirmation",
                    )
                    .await;
                    return Err(OakError::Server(format!(
                        "Failed to confirm chunk uploads: {}",
                        detail
                    )));
                }
            }
        } else {
            push_info("All chunks already on server, skipping upload");
        }
    }

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
    let branch_data = if let Some(name) = branch_name {
        repo.get_branch(name)?.as_ref().map(branch_to_push_data)
    } else {
        None
    };

    // Staged publication uses a structurally versioned endpoint. Never send
    // staging semantics to legacy `/push`: an old mixed-deployment replica
    // could ignore unknown fields and expose an intermediate side-line head.
    let request_path = if staged {
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
    let push_resp: PushResponse = if staged {
        let (stage_id, _, finalize, expected_head, target_head) =
            preplanned_control.expect("staged request has preplanned control");
        let branch = branch_data.ok_or_else(|| {
            OakError::InvalidArgument(
                "staged push requires explicit branch metadata; no remote state was mutated"
                    .to_string(),
            )
        })?;
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
        let deadline = tokio::time::Instant::now() + staged_publication_overall_timeout(body.len());
        let request = with_auth(
            client
                .post(&request_path)
                .header("x-oak-user", super::commit::get_author())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            api_key,
        );
        send_staged_publication_with_cap(request, deadline, STAGED_PUBLICATION_REQUEST_TIMEOUT)
            .await?
    } else {
        let response = with_auth(
            client
                .post(&request_path)
                .header("x-oak-user", super::commit::get_author())
                .json(&PushRequest {
                    expected_head: if force {
                        None
                    } else {
                        remote_head.map(|hash| hash.to_string())
                    },
                    expected_branch_head: None,
                    force,
                    branch: branch_data,
                    commits: commit_data,
                    blobs,
                    trees,
                }),
            api_key,
        )
        .send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
        output::vlog(&format!(
            "push: publication returned {} in {:.3}s",
            response.status(),
            t_push.elapsed().as_secs_f64()
        ));
        if !response.status().is_success() {
            return Err(crate::http::server_error(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| OakError::Http(error.to_string()))?
    };
    output::vlog(&format!(
        "push: publication completed in {:.3}s",
        t_push.elapsed().as_secs_f64()
    ));

    if push_resp.success {
        if !emit_result {
            return Ok(EndpointPushOutcome {
                published: intended_published_head.is_some(),
                current_branch_pushed_head: intended_published_head,
            });
        }
        if quiet_stdout() {
            output::print_line(&format!("Pushed to {remote}"));
        } else if let (Some(repo_path), Some(branch)) = (repo_path_for_web, branch_name) {
            let url = super::branch_web_url(remote, repo_path, branch);
            output::success(&format!("Push complete: {url}"));
        } else {
            output::success("Push complete");
        }
    } else {
        return Err(OakError::ConflictDetected);
    }

    // First push to a freshly-created repo: the push lands on the user's
    // personal/feature branch, but `main` stays empty. The web UI's Code
    // tab and any Pages site then look empty — which reads like the push
    // silently failed. Offer to land the work on main right here so the
    // user doesn't have to discover the squash-merge step on their own.
    if created_repo {
        if let Some(name) = branch_name {
            let parented_on_main = repo
                .get_branch(name)
                .ok()
                .flatten()
                .and_then(|b| b.parent_branch)
                .is_some_and(|p| p == "main");
            if parented_on_main {
                // The merge resets the working tree, so it needs the workdir
                // lock. If another oak process already owns it — typically the
                // `oak merge` that initiated this very push — that process is
                // about to merge itself, so skip the offer entirely.
                let lock = crate::resolve::resolve(work_tree)
                    .ok()
                    .and_then(|ctx| crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir).ok());
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
                        super::merge::merge_to_main_via_server(
                            &lock, repo, work_tree, name, None, false, false,
                        )
                        .await?;
                    }
                } else if !is_interactive() {
                    output::info(
                        "Run `oak merge` to land this on main — otherwise the repo will look empty in the web UI.",
                    );
                }
            }
        }
    }

    Ok(EndpointPushOutcome {
        published: intended_published_head.is_some(),
        current_branch_pushed_head: intended_published_head,
    })
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
            // `oak pull` heals fully. A dirty tree is carried untouched
            // (the `oak switch -c` semantics) and needs no lock.
            let lock = crate::resolve::resolve(work_tree)
                .ok()
                .and_then(|ctx| crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir).ok());
            if worktree_clean && lock.is_none() {
                output::vlog(
                    "push: self-heal needs the workdir lock for the worktree reset; deferring",
                );
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
/// Parses and validates the spec, persists `RepoOwner` + `RepoName`, and lets
/// the downstream push auto-create the repo on the server when it 404s. ORG
/// must already be an organization slug you can push to.
fn link_remote_identity(
    repo: &SqliteRepository,
    remote: &str,
    spec: &str,
) -> Result<(String, String)> {
    let (owner, name) = super::parse_owner_repo(spec).map_err(|err| {
        OakError::InvalidArgument(format!(
            "Invalid --repo value '{spec}'. Use ORG/REPO. {err}"
        ))
    })?;

    repo.set_metadata(MetadataKey::RepoOwner, &owner)?;
    repo.set_metadata(MetadataKey::RepoName, &name)?;
    push_success(&format!("Linked to {owner}/{name} on {remote}"));
    Ok((owner, name))
}

/// Interactive first-push setup: pick or create an organization, choose a repo
/// name, persist `RepoOwner` + `RepoName` to local metadata. The downstream
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
        return Err(OakError::Config(format!(
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

    let token = super::credentials::effective_token(remote, api_key.map(String::from));
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
            super::credentials::effective_token(remote, None).ok_or_else(|| {
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

    repo.set_metadata(MetadataKey::RepoOwner, &owner)?;
    repo.set_metadata(MetadataKey::RepoName, &name)?;
    output::success(&format!("Linked to {owner}/{name} on {remote}"));
    output::blank();

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
    use std::io::{Read, Seek, SeekFrom};

    use super::{
        admit_local_blob_descriptor, best_effort_abort_staged_session,
        best_effort_abort_staged_session_with_timeout, collect_planned_objects, commit_to_wire,
        decode_chunk_response_json, link_remote_identity, materialize_planned_blob_batch,
        plan_outgoing_commits, prepare_staged_mapping_proofs, prove_mapping_set_once,
        remote_missing_staged_blobs, reserve_rechunk_file, select_staged_protocol,
        send_chunk_request_with_busy_retry, send_mapping_proof_request_with_cap,
        send_presigned_chunk_put, send_staged_publication_with_cap, server_push_capability,
        split_staged_blob_batches, split_staged_tree_batches_with_limits, staged_protocol_required,
        staged_session_capability_available, upload_mapping_set_once,
        upload_mapping_set_with_restarts, validate_blob_check_missing,
        validate_chunk_check_missing, validate_external_edge_proofs, validate_push_operation_caps,
        validate_receipt_predicate_ack, ChunkBusyRetryState, PlannedBlob, PreparedChunk,
        PreparedMappingProof, PushOperationTotals, RechunkWorkspace, ServerPushCapability,
        BOOTSTRAP_BATCH_SIZE, STAGED_PUBLICATION_MAX_ATTEMPTS,
        STAGED_PUBLICATION_RESPONSE_MAX_BYTES,
    };
    use oak_core::{
        hash_bytes,
        protocol::{
            tree_to_wire, BlobCheckResponse, BlobData, BranchPushData, ChunkRefData as ChunkRef,
            ChunkUploadInfo, PushResponse, StagedPushRequest,
        },
        Blob, Branch, ChunkInfo, Commit, FileMode, Hash, ManifestEntry, MetadataKey, OakError,
        Repository, SqliteRepository, Tree, TreeEntry, TreeEntryKind,
    };

    fn temp_repo() -> (tempfile::TempDir, SqliteRepository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
        (dir, repo)
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

    #[test]
    fn chunk_check_missing_must_be_a_unique_subset_of_the_request_page() {
        let requested = vec!["aa".repeat(32), "bb".repeat(32)];
        let upload = |hash: String| ChunkUploadInfo {
            hash,
            upload_url: None,
        };

        let unknown =
            validate_chunk_check_missing(&requested, vec![upload("cc".repeat(32))], "chunk check")
                .err()
                .unwrap();
        assert!(unknown.to_string().contains("unrequested hash"));

        let duplicate = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[0].clone()), upload(requested[0].clone())],
            "chunk check",
        )
        .err()
        .unwrap();
        assert!(duplicate.to_string().contains("duplicate hash"));

        let mixed = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[0].clone()), upload("cc".repeat(32))],
            "chunk check",
        )
        .err()
        .unwrap();
        assert!(mixed.to_string().contains("unrequested hash"));

        let valid = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[1].clone())],
            "chunk check",
        )
        .unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].hash, requested[1]);
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
    async fn staged_publication_replays_exact_request_when_final_response_body_is_withheld() {
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
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let bodies_tx = bodies_tx.clone();
                tokio::spawn(async move {
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
                    if attempt == 0 {
                        stream.write_all(b"{").await.unwrap();
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    } else {
                        stream.write_all(body).await.unwrap();
                    }
                });
            }
        });

        let request_body = br#"{"stage_id":"same-request"}"#.to_vec();
        let response = send_staged_publication_with_cap(
            crate::http::api_client()
                .post(format!("http://{address}/api/oak/oak/push/staged-v1"))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone()),
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert!(response.success);
        assert_eq!(bodies_rx.recv().await.unwrap(), request_body);
        assert_eq!(bodies_rx.recv().await.unwrap(), request_body);
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
            .expect(STAGED_PUBLICATION_MAX_ATTEMPTS as u64)
            .mount(&server)
            .await;

        let result = send_staged_publication_with_cap(
            crate::http::api_client()
                .post(format!("{}/api/oak/oak/push/staged-v1", server.uri()))
                .json(&serde_json::json!({"stage_id": "same-request"})),
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(1),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("oversized staged response must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exceeded 65536 bytes"));
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

    #[tokio::test(flavor = "current_thread")]
    async fn chunk_check_replays_exact_payload_after_typed_object_busy_conflict() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "hashes": ["a".repeat(64)],
            "sizes": [3],
            "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL,
        });
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "0")
                    .set_body_string("admission busy"),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(
                ResponseTemplate::new(409)
                    .insert_header("Retry-After", "1")
                    .set_body_json(serde_json::json!({
                        "error": "chunk_object_busy",
                        "retry_after_ms": 250
                    })),
            )
            .with_priority(2)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": []
            })))
            .with_priority(3)
            .expect(1)
            .mount(&server)
            .await;

        let response = send_chunk_request_with_busy_retry(
            crate::http::api_client()
                .post(format!("{}/api/oak/oak/chunks/check", server.uri()))
                .header("authorization", "Bearer secret")
                .json(&body),
            std::sync::Arc::new(ChunkBusyRetryState::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                std::time::Duration::from_secs(3),
            )),
            "chunk check",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chunk_busy_retry_never_sleeps_past_remaining_overall_deadline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/busy"))
            .respond_with(
                ResponseTemplate::new(409)
                    .insert_header("Retry-After", "1")
                    .set_body_json(serde_json::json!({
                        "error": "chunk_object_busy",
                        "retry_after_ms": 250
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        let error = send_chunk_request_with_busy_retry(
            crate::http::api_client().post(format!("{}/busy", server.uri())),
            std::sync::Arc::new(ChunkBusyRetryState::new(
                tokio::time::Instant::now() + std::time::Duration::from_millis(100),
                std::time::Duration::from_secs(60),
            )),
            "chunk upload",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("busy-wait budget"));
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_never_busy_chunk_request_does_not_consume_busy_wait_budget() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = std::sync::Arc::new(ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::ZERO,
        ));
        let response = send_chunk_request_with_busy_retry(
            crate::http::api_client().put(format!("{}/slow", server.uri())),
            state.clone(),
            "chunk upload",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            *state.busy_wait_reserved.lock().await,
            std::time::Duration::ZERO
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_chunk_check_body_is_bounded_by_the_overall_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1024\r\n\r\n{",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        let response = crate::http::api_client()
            .get(format!("http://{address}/withheld-body"))
            .send()
            .await
            .unwrap();
        let state = ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_millis(40),
            std::time::Duration::ZERO,
        );
        let error =
            decode_chunk_response_json::<serde_json::Value>(response, &state, "chunk check")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("response body exceeded"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_presigned_put_uses_transfer_deadline_not_busy_budget() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/presigned"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::ZERO,
        );
        let response = send_presigned_chunk_put(
            &crate::http::api_client(),
            &format!("{}/presigned", server.uri()),
            vec![7; 1024],
            &state,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            *state.busy_wait_reserved.lock().await,
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn link_remote_identity_persists_valid_owner_and_repo() {
        let (_dir, repo) = temp_repo();

        let linked = link_remote_identity(&repo, "https://oak.space", "oak/benchmarks").unwrap();

        assert_eq!(linked, ("oak".to_string(), "benchmarks".to_string()));
        assert_eq!(
            repo.get_metadata(MetadataKey::RepoOwner)
                .unwrap()
                .as_deref(),
            Some("oak")
        );
        assert_eq!(
            repo.get_metadata(MetadataKey::RepoName).unwrap().as_deref(),
            Some("benchmarks")
        );
    }

    #[test]
    fn link_remote_identity_rejects_unsafe_specs_without_persisting() {
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
            let (_dir, repo) = temp_repo();
            assert!(
                link_remote_identity(&repo, "https://oak.space", spec).is_err(),
                "expected {spec:?} to be rejected"
            );
            assert_eq!(repo.get_metadata(MetadataKey::RepoOwner).unwrap(), None);
            assert_eq!(repo.get_metadata(MetadataKey::RepoName).unwrap(), None);
        }
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
    fn rechunk_workspace_is_reserved_before_use_and_removed_on_drop() {
        let blob_size = 1024 * 1024;
        let (directory, repo) = temp_repo();
        let mut workspace = RechunkWorkspace::create(&repo, blob_size).unwrap();
        let source_path = workspace.source.path().to_path_buf();
        let capacity_path = workspace.persistence_reservation.path().to_path_buf();
        assert_eq!(
            source_path.parent().unwrap().canonicalize().unwrap(),
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(
            capacity_path.parent().unwrap().canonicalize().unwrap(),
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(
            workspace.source.as_file().metadata().unwrap().len(),
            blob_size
        );
        assert_eq!(
            workspace
                .persistence_reservation
                .as_file()
                .metadata()
                .unwrap()
                .len(),
            blob_size * 2
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(workspace.source.as_file().metadata().unwrap().blocks() > 0);
            assert!(
                workspace
                    .persistence_reservation
                    .as_file()
                    .metadata()
                    .unwrap()
                    .blocks()
                    > 0
            );
        }
        workspace
            .source_file_mut()
            .seek(SeekFrom::Start(0))
            .unwrap();
        let mut source = workspace.source_file_mut().take(blob_size);
        assert_eq!(
            std::io::copy(&mut source, &mut std::io::sink()).unwrap(),
            blob_size
        );
        drop(workspace);
        assert!(!source_path.exists());
        assert!(!capacity_path.exists());
    }

    #[test]
    fn rechunk_workspace_cleans_up_after_repository_local_enospc() {
        let (directory, repo) = temp_repo();
        let mut reservations = 0;
        let error = RechunkWorkspace::create_with_reserver(&repo, 1024, |file, bytes| {
            reservations += 1;
            if reservations == 2 {
                Err(OakError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOSPC,
                )))
            } else {
                reserve_rechunk_file(file, bytes)
            }
        })
        .err()
        .expect("persistence reservation must fail closed");
        assert!(error.to_string().contains("No space left"));
        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".oak-rechunk-")
                    && entry.file_name() != ".oak-rechunk.lock"
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary reservations leaked: {leftovers:?}"
        );
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

    #[test]
    fn planned_large_blob_retains_only_chunk_locations() {
        const CHUNKS: usize = 12;
        const CHUNK_BYTES: usize = 1024 * 1024;
        let (_dir, repo) = temp_repo();
        let mut content = Vec::with_capacity(CHUNKS * CHUNK_BYTES);
        let mut mapping = Vec::new();
        for index in 0..CHUNKS {
            let bytes = vec![index as u8; CHUNK_BYTES];
            let hash = hash_bytes(&bytes);
            repo.store_chunk(&hash, &bytes).unwrap();
            mapping.push(ChunkInfo {
                hash,
                offset: (index * CHUNK_BYTES) as u64,
                length: CHUNK_BYTES as u32,
            });
            content.extend_from_slice(&bytes);
        }
        let blob_hash = hash_bytes(&content);
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content: Vec::new(),
            size: content.len() as u64,
        })
        .unwrap();
        repo.store_blob_chunks(&blob_hash, &mapping).unwrap();
        let tree = Tree::new(vec![TreeEntry {
            name: "large.bin".to_string(),
            kind: TreeEntryKind::Blob,
            hash: blob_hash,
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
        drop(content);

        let plan = collect_planned_objects(&repo, &[&commit]).unwrap();
        assert_eq!(plan.blobs.len(), 1);
        let materialized = materialize_planned_blob_batch(&repo, plan.blobs).unwrap();
        assert_eq!(materialized.chunks.len(), CHUNKS);
        assert!(materialized
            .chunks
            .iter()
            .all(|chunk| matches!(chunk, PreparedChunk::Stored(_))));
        assert_eq!(
            std::mem::size_of_val(materialized.chunks.as_slice()),
            CHUNKS * std::mem::size_of::<PreparedChunk>(),
            "the 12 MiB fixture must retain descriptors, not a second byte copy"
        );
    }

    #[test]
    fn staged_mapping_preflight_rechunks_before_network_payloads_exist() {
        let (_dir, repo) = temp_repo();
        let parts = [
            vec![b'a'; 300_000],
            vec![b'b'; 300_000],
            vec![b'c'; 300_000],
        ];
        let content: Vec<u8> = parts.iter().flatten().copied().collect();
        let blob_hash = hash_bytes(&content);
        let mut offset = 0u64;
        let mut mapping = Vec::new();
        for bytes in &parts {
            let hash = hash_bytes(bytes);
            repo.store_chunk(&hash, bytes).unwrap();
            mapping.push(ChunkInfo {
                hash,
                offset,
                length: bytes.len() as u32,
            });
            offset += bytes.len() as u64;
        }
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content: Vec::new(),
            size: content.len() as u64,
        })
        .unwrap();
        repo.store_blob_chunks(&blob_hash, &mapping).unwrap();

        let mut blobs = vec![BlobData {
            hash: blob_hash.to_string(),
            content: Vec::new(),
            size: content.len() as u64,
            chunks: mapping
                .iter()
                .map(|chunk| ChunkRef {
                    hash: chunk.hash.to_string(),
                    offset: chunk.offset,
                    size: chunk.length,
                })
                .collect(),
            mapping_proof_token: None,
        }];
        let mut sources = mapping
            .iter()
            .map(|chunk| {
                PreparedChunk::Stored(ChunkRef {
                    hash: chunk.hash.to_string(),
                    offset: chunk.offset,
                    size: chunk.length,
                })
            })
            .collect();

        let proofs = prepare_staged_mapping_proofs(&repo, &mut blobs, &mut sources, 2).unwrap();

        assert_eq!(proofs.len(), 1);
        assert!(proofs[0].chunks.len() <= 2);
        assert_eq!(
            proofs[0].descriptor.total_chunks as usize,
            proofs[0].chunks.len()
        );
        assert_eq!(sources.len(), proofs[0].chunks.len());
        let stored = repo.get_blob_chunks(&blob_hash).unwrap().unwrap();
        assert_eq!(stored.len(), proofs[0].chunks.len());
        let rebuilt: Vec<u8> = stored
            .iter()
            .flat_map(|chunk| repo.get_chunk(&chunk.hash).unwrap().unwrap())
            .collect();
        assert_eq!(hash_bytes(&rebuilt), blob_hash);
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

    #[tokio::test(flavor = "current_thread")]
    async fn one_commit_over_tree_batch_cap_stages_objects_then_finalizes() {
        let bytes = b"bounded object staging".to_vec();
        let blob_hash = hash_bytes(&bytes);
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
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            root.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
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

        let served = tempfile::tempdir().unwrap();
        let base = crate::commands::serve::spawn_loopback(served.path().to_path_buf())
            .await
            .unwrap();
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let request = |commits, trees, blobs| StagedPushRequest {
            stage_id: "0123456789abcdef0123456789abcdef".to_string(),
            expected_branch_head: None,
            branch: BranchPushData {
                name: "main".to_string(),
                description: None,
                parent_branch: None,
                status: "open".to_string(),
                created_at: "2026-08-31T00:00:00Z".to_string(),
                close_reason: None,
            },
            finalize: false,
            force: false,
            target_head: None,
            commits,
            blobs,
            trees,
        };
        let client = crate::http::api_client();
        let response = client
            .post(format!("{base}/api/repos"))
            .json(&serde_json::json!({
                "name": "oak",
                "description": null,
                "organization_slug": "oak"
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let response = client
            .post(&endpoint)
            .json(&request(
                vec![],
                vec![],
                vec![BlobData {
                    hash: blob_hash.to_string(),
                    content: bytes.clone(),
                    size: bytes.len() as u64,
                    chunks: vec![],
                    mapping_proof_token: None,
                }],
            ))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        for batch in tree_batches {
            let response = client
                .post(&endpoint)
                .json(&request(vec![], batch.trees, vec![]))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
        }
        let response = client
            .post(&endpoint)
            .json(&request(vec![commit_to_wire(&commit)], vec![], vec![]))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let mut final_request = request(vec![], vec![], vec![]);
        final_request.finalize = true;
        final_request.target_head = Some(commit.hash.to_string());
        let response = client
            .post(&endpoint)
            .json(&final_request)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let published: serde_json::Value = client
            .get(format!("{base}/api/oak/oak/branches/main"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(published["head"], commit.hash.as_str());
    }
}
