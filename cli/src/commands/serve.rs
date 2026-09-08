//! `oak serve` — a minimal, self-hostable Oak server backed by SQLite.
//!
//! This is the answer to "I want to host my own Oak repos without running the
//! whole oak.space stack." It speaks the same push/pull/clone wire protocol the
//! hosted server speaks (the shared [`oak_core::protocol`] types), but with a
//! deliberately tiny surface:
//!
//!   * **Storage is SQLite + the local blob store** — one `*.oakdb` file per
//!     repo under `--dir`, opened through [`oak_core::SqliteRepository`] (the
//!     exact store the `oak` CLI uses locally). No PostgreSQL, no R2.
//!   * **No organizations, users, or auth model.** Repos are addressed as
//!     `{owner}/{name}` purely as a directory namespace — `owner` carries no
//!     permission meaning. An optional `--token` shared secret is the only
//!     access control. The server binds loopback by default; widening the bind
//!     address requires a token.
//!
//! Point the CLI at it with `oak clone <host>/<owner>/<name>`, `oak push`,
//! `oak pull` — unchanged. Because this server never configures R2, every
//! chunk-check response sets `upload_url: None`, which routes the client onto
//! the server-mediated `PUT /chunks/{hash}` + inline-download path it already
//! supports in production. No client changes are required.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path as AxPath, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use oak_core::protocol::{
    tree_data_to_core, tree_to_wire, validate_staged_closure, BlobCheckRequest, BlobCheckResponse,
    BlobData, BlobProofChunk, BlobProofDescriptor, BlobProofFinalizeRequest, BlobProofPagesRequest,
    BlobProofPagesResponse, BlobProofRequest, BlobProofResponse, BranchPullData, BranchPushData,
    ChunkCheckRequest, ChunkCheckServerResponse, ChunkDownloadInfo, ChunkDownloadRequest,
    ChunkDownloadResponse, ChunkRefData, ChunkUploadInfo, ChunkUploadedRequest, CommitData,
    CommitInfoRequest, CommitInfoResponse, CreateRepoRequest, ErrorResponse, FileChangeData,
    MappingProofJob, PullQuery, PullResponse, PushRequest, PushResponse, RepoResponse,
    StagedAbortRequest, StagedAbortResponse, StagedPushRequest,
};
use oak_core::{
    collect_tree_objects, hash_bytes, Blob, Branch, BranchStatus, ChangeType, ChunkInfo,
    CloseReason, Commit, Hash, MetadataKey, OakError, Repository, Result, SqliteRepository, Tree,
};

/// Shared server state. Cheap to clone (a path + an `Arc`), as axum requires.
#[derive(Clone)]
struct ServeState {
    /// Directory holding one `<owner>/<name>.oakdb` per repo.
    root: PathBuf,
    /// Optional shared bearer token. When `Some`, every request must send
    /// `Authorization: Bearer <token>`. When `None`, the server is open.
    token: Option<Arc<String>>,
}

const STAGED_SESSION_TTL_SECS: i64 = oak_core::protocol::STAGED_ACTIVE_SESSION_TTL_SECS;
const MAX_TREE_OBJECTS: usize = oak_core::protocol::STAGED_MAX_TREE_OBJECTS;
const MAX_DIRECT_TREE_ENTRIES: usize = oak_core::protocol::STAGED_MAX_DIRECT_TREE_ENTRIES;
const MAX_FILE_CHANGES: usize = oak_core::protocol::STAGED_MAX_FILE_CHANGES;
const MAX_METADATA_BYTES: usize = oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES;
const MAX_BLOBS: usize = oak_core::protocol::STAGED_MAX_BLOBS;
const MAX_BLOB_BYTES: u64 = oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES;
const MAX_COMPLETED_STAGE_TOMBSTONES: usize = 256;

fn staged_branch_identity(branch: &BranchPushData) -> std::result::Result<String, ServeError> {
    let bytes = serde_json::to_vec(branch)
        .map_err(|error| ServeError::bad_request(format!("invalid staged branch data: {error}")))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn validate_object_envelope_caps(
    commits: &[CommitData],
    trees: &[oak_core::protocol::TreeData],
    blobs: &[BlobData],
    branch: &BranchPushData,
    max_commits: usize,
) -> std::result::Result<(), ServeError> {
    let direct_entries = trees.iter().map(|tree| tree.entries.len()).sum::<usize>();
    let file_changes = commits
        .iter()
        .map(|commit| commit.files.len())
        .sum::<usize>();
    let blob_bytes = blobs
        .iter()
        .fold(0u64, |total, blob| total.saturating_add(blob.size));
    let chunk_refs = blobs.iter().fold(0usize, |total, blob| {
        total.saturating_add(blob.chunks.len())
    });
    let metadata = oak_core::protocol::staged_branch_metadata_bytes(branch).saturating_add(
        commits
            .iter()
            .fold(0usize, |total, commit| {
                total.saturating_add(oak_core::protocol::staged_commit_metadata_bytes(commit))
            })
            .saturating_add(trees.iter().fold(0usize, |total, tree| {
                total.saturating_add(oak_core::protocol::staged_tree_metadata_bytes(tree))
            }))
            .saturating_add(blobs.iter().fold(0usize, |total, blob| {
                total.saturating_add(oak_core::protocol::staged_blob_metadata_bytes(blob))
            })),
    );
    if commits.len() > max_commits
        || trees.len() > MAX_TREE_OBJECTS
        || direct_entries > MAX_DIRECT_TREE_ENTRIES
        || file_changes > MAX_FILE_CHANGES
        || blobs.len() > MAX_BLOBS
        || blob_bytes > MAX_BLOB_BYTES
        || chunk_refs > oak_core::protocol::STAGED_MAX_CHUNK_REFS
        || metadata > MAX_METADATA_BYTES
    {
        return Err(ServeError::bad_request(
            "object envelope exceeds server admission caps",
        ));
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ServeStageState {
    #[default]
    Active,
    Finalizing,
    Completed,
    Aborted,
}

fn stage_state_is_terminal(state: ServeStageState) -> bool {
    matches!(state, ServeStageState::Completed | ServeStageState::Aborted)
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct ServeStageTotals {
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

fn staged_totals_are_empty(totals: &ServeStageTotals) -> bool {
    *totals == ServeStageTotals::default()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ServeStageSession {
    branch: String,
    #[serde(default)]
    branch_identity: String,
    expected_head: Option<String>,
    #[serde(default)]
    force: bool,
    updated_at: i64,
    #[serde(default)]
    state: ServeStageState,
    #[serde(default)]
    completed_target: Option<String>,
    #[serde(default, skip_serializing_if = "staged_totals_are_empty")]
    totals: ServeStageTotals,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    manifest_roots: HashSet<String>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    commits: HashSet<String>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    trees: HashSet<String>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    blobs: HashSet<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    blob_mappings: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    chunks: HashSet<String>,
}

fn load_active_stage_sessions(
    repo: &SqliteRepository,
) -> Result<HashMap<String, ServeStageSession>> {
    let raw = repo
        .get_metadata(MetadataKey::StagedPushV1Sessions)?
        .unwrap_or_default();
    let mut sessions: HashMap<String, ServeStageSession> = if raw.trim().is_empty() {
        HashMap::new()
    } else {
        serde_json::from_str(&raw).map_err(|error| {
            OakError::Database(format!("invalid staged session metadata: {error}"))
        })?
    };
    let now = chrono::Utc::now().timestamp();
    // Expiry drops only ownership receipts. Terminal IDs are fenced for the
    // advertised seven-day/256-receipt replay window, not forever. Immutable
    // content-addressed objects are retained: deleting content shared with
    // another session or published commit would be unsafe without references.
    sessions.retain(|_, session| {
        let ttl = if stage_state_is_terminal(session.state) {
            oak_core::protocol::STAGED_COMPLETED_TOMBSTONE_TTL_SECS
        } else {
            STAGED_SESSION_TTL_SECS
        };
        session.updated_at >= now - ttl
    });
    prune_completed_stage_tombstones(&mut sessions);
    Ok(sessions)
}

fn prune_completed_stage_tombstones(sessions: &mut HashMap<String, ServeStageSession>) {
    let mut completed: Vec<(String, i64)> = sessions
        .iter()
        .filter(|(_, session)| stage_state_is_terminal(session.state))
        .map(|(stage_id, session)| (stage_id.clone(), session.updated_at))
        .collect();
    completed
        .sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (stage_id, _) in completed.into_iter().skip(MAX_COMPLETED_STAGE_TOMBSTONES) {
        sessions.remove(&stage_id);
    }
}

fn compact_completed_stage_session(
    session: &mut ServeStageSession,
    target: String,
    completed_at: i64,
) {
    session.state = ServeStageState::Completed;
    session.completed_target = Some(target);
    session.updated_at = completed_at;
    session.totals = ServeStageTotals::default();
    session.manifest_roots.clear();
    session.commits.clear();
    session.trees.clear();
    session.blobs.clear();
    session.blob_mappings.clear();
    session.chunks.clear();
}

fn compact_aborted_stage_session(session: &mut ServeStageSession, aborted_at: i64) {
    session.state = ServeStageState::Aborted;
    session.completed_target = None;
    session.updated_at = aborted_at;
    session.totals = ServeStageTotals::default();
    session.manifest_roots.clear();
    session.commits.clear();
    session.trees.clear();
    session.blobs.clear();
    session.blob_mappings.clear();
    session.chunks.clear();
}

fn store_stage_sessions(
    repo: &SqliteRepository,
    sessions: &HashMap<String, ServeStageSession>,
) -> Result<()> {
    let encoded = serde_json::to_string(sessions)
        .map_err(|error| OakError::Database(format!("encode staged sessions: {error}")))?;
    repo.set_metadata(MetadataKey::StagedPushV1Sessions, &encoded)
}

fn validate_stage_operation_totals(
    totals: &ServeStageTotals,
) -> std::result::Result<(), ServeError> {
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
    if let Some((dimension, actual, limit)) =
        checks.into_iter().find(|(_, actual, limit)| actual > limit)
    {
        return Err(ServeError::bad_request(format!(
            "staged operation {dimension} total {actual} exceeds limit {limit}"
        )));
    }
    if totals.declared_blob_bytes > oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES {
        return Err(ServeError::bad_request(format!(
            "staged operation declared blob bytes total {} exceeds limit {}",
            totals.declared_blob_bytes,
            oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES
        )));
    }
    Ok(())
}

fn manifest_size_for_stage(
    repo: &SqliteRepository,
    root: &Hash,
    incoming: &HashMap<Hash, Tree>,
) -> Result<(usize, usize)> {
    if *root == Tree::empty_hash() {
        return Ok((0, 0));
    }
    let mut entries = 0usize;
    let mut path_bytes = 0usize;
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((hash, prefix_bytes)) = stack.pop() {
        let tree = match incoming.get(&hash) {
            Some(tree) => tree.clone(),
            None => repo
                .get_tree(&hash)?
                .ok_or_else(|| OakError::ManifestNotFound(hash.to_string()))?,
        };
        for entry in tree.entries {
            let full_path_bytes = prefix_bytes
                .saturating_add(usize::from(prefix_bytes != 0))
                .saturating_add(entry.name.len());
            path_bytes = path_bytes.saturating_add(full_path_bytes);
            if entry.kind == oak_core::TreeEntryKind::Tree {
                stack.push((entry.hash, full_path_bytes));
            } else {
                entries = entries.saturating_add(1);
            }
        }
    }
    Ok((entries, path_bytes))
}

fn updated_stage_session(
    repo: &SqliteRepository,
    req: &StagedPushRequest,
    admitted: &oak_core::protocol::ValidatedStagedClosure,
    existing: Option<&ServeStageSession>,
) -> std::result::Result<ServeStageSession, ServeError> {
    let branch_identity = staged_branch_identity(&req.branch)?;
    let mut session = existing.cloned().unwrap_or_else(|| ServeStageSession {
        branch: req.branch.name.clone(),
        branch_identity,
        expected_head: req.expected_branch_head.clone(),
        force: req.force,
        state: ServeStageState::Active,
        totals: ServeStageTotals {
            canonical_metadata_bytes: oak_core::protocol::staged_branch_metadata_bytes(&req.branch),
            ..ServeStageTotals::default()
        },
        ..ServeStageSession::default()
    });
    let incoming_trees: HashMap<Hash, Tree> = admitted
        .trees
        .iter()
        .map(|tree| (tree.hash.clone(), tree.clone()))
        .collect();
    for (wire, commit) in req.commits.iter().zip(&admitted.commits) {
        if session.commits.insert(commit.hash.to_string()) {
            session.totals.commits = session.totals.commits.saturating_add(1);
            session.totals.file_changes = session
                .totals
                .file_changes
                .saturating_add(commit.files.len());
            session.totals.canonical_metadata_bytes = session
                .totals
                .canonical_metadata_bytes
                .saturating_add(oak_core::protocol::staged_commit_metadata_bytes(wire));
        }
        if session
            .manifest_roots
            .insert(commit.manifest_hash.to_string())
        {
            let (entries, paths) =
                manifest_size_for_stage(repo, &commit.manifest_hash, &incoming_trees)?;
            session.totals.resolved_manifest_entries = session
                .totals
                .resolved_manifest_entries
                .saturating_add(entries);
            session.totals.expanded_path_bytes =
                session.totals.expanded_path_bytes.saturating_add(paths);
        }
    }
    for (wire, tree) in req.trees.iter().zip(&admitted.trees) {
        if session.trees.insert(tree.hash.to_string()) {
            session.totals.trees = session.totals.trees.saturating_add(1);
            session.totals.direct_tree_entries = session
                .totals
                .direct_tree_entries
                .saturating_add(tree.entries.len());
            session.totals.canonical_metadata_bytes = session
                .totals
                .canonical_metadata_bytes
                .saturating_add(oak_core::protocol::staged_tree_metadata_bytes(wire));
        }
    }
    for (wire, blob) in req.blobs.iter().zip(&admitted.blobs) {
        let hash = blob.blob.hash.to_string();
        let mapping_digest = canonical_blob_mapping_digest(&blob.chunks);
        if let Some(existing) = session.blob_mappings.get(&hash) {
            if existing != &mapping_digest {
                return Err(ServeError::bad_request(format!(
                    "blob {hash} was already reserved with a different canonical chunk mapping"
                )));
            }
        } else {
            session.blob_mappings.insert(hash.clone(), mapping_digest);
        }
        if session.blobs.insert(hash) {
            session.totals.blobs = session.totals.blobs.saturating_add(1);
            session.totals.declared_blob_bytes = session
                .totals
                .declared_blob_bytes
                .saturating_add(blob.blob.size);
            session.totals.chunk_refs = session.totals.chunk_refs.saturating_add(blob.chunks.len());
            session.totals.canonical_metadata_bytes = session
                .totals
                .canonical_metadata_bytes
                .saturating_add(oak_core::protocol::staged_blob_metadata_bytes(wire));
        }
        session
            .chunks
            .extend(blob.chunks.iter().map(|chunk| chunk.hash.to_string()));
    }
    session.updated_at = chrono::Utc::now().timestamp();
    validate_stage_operation_totals(&session.totals)?;
    Ok(session)
}

fn canonical_blob_mapping_digest(chunks: &[ChunkInfo]) -> String {
    let wire: Vec<BlobProofChunk> = chunks
        .iter()
        .map(|chunk| BlobProofChunk {
            hash: chunk.hash.to_string(),
            offset: chunk.offset,
            size: chunk.length,
        })
        .collect();
    oak_core::protocol::blob_mapping_digest(&wire)
}

type ServeMappingProofJob = oak_core::ServeMappingProofRecord;

fn serve_mapping_proof_request_digest(
    owner: &str,
    name: &str,
    descriptors: &[BlobProofDescriptor],
    published: &PublishedObjectClosure,
    repo: &SqliteRepository,
) -> std::result::Result<String, ServeError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"oak-serve-mapping-proof-async-v1\0");
    hasher.update(owner.as_bytes());
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        &serde_json::to_vec(descriptors)
            .map_err(|error| ServeError::bad_request(error.to_string()))?,
    );
    for descriptor in descriptors {
        hasher.update(b"\0base\0");
        if published.blobs.contains(&descriptor.hash) {
            let hash = Hash::from_hex(&descriptor.hash)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
            let mapping = repo.get_blob_chunks(&hash)?.unwrap_or_default();
            hasher.update(canonical_blob_mapping_digest(&mapping).as_bytes());
        } else {
            hasher.update(b"none");
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn mapping_proof_response(job: &ServeMappingProofJob) -> BlobProofResponse {
    if job.status == "complete" {
        BlobProofResponse {
            verified: job.verified.clone(),
            missing: job.missing.clone(),
            proof_token: job.missing.is_empty().then(|| job.token.clone()),
            mapping_proof_job: None,
        }
    } else {
        BlobProofResponse {
            verified: Vec::new(),
            missing: Vec::new(),
            proof_token: None,
            mapping_proof_job: Some(MappingProofJob {
                token: job.token.clone(),
                status: job.status.clone(),
                retry_after_ms: 500,
            }),
        }
    }
}

fn mapping_proof_http_status(
    repo: &SqliteRepository,
    job: &ServeMappingProofJob,
) -> std::result::Result<StatusCode, ServeError> {
    if job.status == "conflict" {
        let code = repo
            .serve_mapping_proof_terminal_code(&job.token)?
            .unwrap_or_else(|| oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT.to_string());
        return Err(ServeError::new(StatusCode::CONFLICT, code));
    }
    Ok(if job.status == "complete" {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    })
}

fn persist_mapping_generation_conflict(
    repo: &SqliteRepository,
    token: &str,
    worker_token: &str,
) -> std::result::Result<(), ServeError> {
    if repo.conflict_claimed_serve_mapping_proof(
        token,
        worker_token,
        oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT,
        chrono::Utc::now().timestamp(),
    )? {
        Ok(())
    } else {
        Err(ServeError::internal(
            "mapping proof worker lost its claim before recording generation conflict",
        ))
    }
}

fn is_mapping_generation_conflict(error: &OakError) -> bool {
    matches!(
        error,
        OakError::InvalidArgument(message)
            if message == "blob acquired a different immutable mapping identity"
                || message == "blob metadata size changed before mapping activation"
    )
}

fn missing_mapping_proof_content_is_now_valid(
    repo: &SqliteRepository,
    job: &ServeMappingProofJob,
) -> Result<bool> {
    let missing: HashSet<&str> = job.missing.iter().map(String::as_str).collect();
    for (blob_index, descriptor) in job.descriptors.iter().enumerate() {
        if !missing.contains(descriptor.hash.as_str()) {
            continue;
        }
        let expected = Hash::from_hex(&descriptor.hash)?;
        let mut hasher = blake3::Hasher::new();
        let mut present = true;
        let mut bytes_seen = 0u64;
        repo.visit_serve_mapping_proof_chunks(&job.token, blob_index as u32, |_, chunk| {
            let chunk_hash = Hash::from_hex(&chunk.hash)?;
            match repo.get_chunk(&chunk_hash)? {
                Some(bytes)
                    if bytes.len() == chunk.size as usize && hash_bytes(&bytes) == chunk_hash =>
                {
                    bytes_seen = bytes_seen.saturating_add(bytes.len() as u64);
                    hasher.update(&bytes);
                }
                _ => present = false,
            }
            Ok(())
        })?;
        if !present
            || bytes_seen != descriptor.size
            || hasher.finalize().to_hex().as_str() != expected.as_str()
        {
            return Ok(false);
        }
    }
    Ok(!missing.is_empty())
}

fn validate_staged_mapping_proof_tokens(
    _owner: &str,
    _name: &str,
    blobs: &[BlobData],
    published: &PublishedObjectClosure,
    repo: &SqliteRepository,
) -> std::result::Result<(), ServeError> {
    let mut groups: HashMap<&str, Vec<&BlobData>> = HashMap::new();
    for blob in blobs {
        if blob.chunks.is_empty() {
            if blob.mapping_proof_token.is_some() {
                return Err(ServeError::bad_request(
                    "inline staged blob must not carry a mapping proof token",
                ));
            }
            continue;
        }
        let token = blob.mapping_proof_token.as_deref().ok_or_else(|| {
            ServeError::bad_request("chunked staged blob requires async_v1 mapping proof token")
        })?;
        groups.entry(token).or_default().push(blob);
    }
    for (token, blobs) in groups {
        let job = repo
            .load_serve_mapping_proof_header(token)?
            .ok_or_else(|| {
                ServeError::new(
                    StatusCode::CONFLICT,
                    "mapping proof token is unknown or expired",
                )
            })?;
        if job.status != "complete" {
            return Err(ServeError::new(
                StatusCode::CONFLICT,
                "mapping proof token is not terminal",
            ));
        }
        for (index, descriptor) in job.descriptors.iter().enumerate() {
            let current = if published.blobs.contains(&descriptor.hash) {
                let hash = Hash::from_hex(&descriptor.hash)
                    .map_err(|error| ServeError::bad_request(error.to_string()))?;
                Some(canonical_blob_mapping_digest(
                    &repo.get_blob_chunks(&hash)?.unwrap_or_default(),
                ))
            } else {
                None
            };
            let base = job.base_mapping_digests.get(index).cloned().flatten();
            if current != base && current.as_deref() != Some(descriptor.mapping_digest.as_str()) {
                return Err(ServeError::new(
                    StatusCode::CONFLICT,
                    "mapping proof live generation changed after verification",
                ));
            }
        }
        for blob in blobs {
            let descriptor = job
                .descriptors
                .iter()
                .find(|descriptor| descriptor.hash == blob.hash)
                .ok_or_else(|| {
                    ServeError::new(
                        StatusCode::CONFLICT,
                        "mapping proof token does not cover this blob",
                    )
                })?;
            let chunks: Vec<BlobProofChunk> = blob
                .chunks
                .iter()
                .map(|chunk| BlobProofChunk {
                    hash: chunk.hash.clone(),
                    offset: chunk.offset,
                    size: chunk.size,
                })
                .collect();
            if descriptor.size != blob.size
                || descriptor.total_chunks as usize != chunks.len()
                || descriptor.mapping_digest != oak_core::protocol::blob_mapping_digest(&chunks)
                || !job.verified.contains(&blob.hash)
            {
                return Err(ServeError::new(
                    StatusCode::CONFLICT,
                    "mapping proof token does not bind this exact blob mapping",
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// A handler error that renders as `(status, {"error": ...})` — the same JSON
/// error shape the hosted server uses, so the CLI's existing error parsing
/// works unchanged.
#[derive(Debug)]
struct ServeError {
    status: StatusCode,
    message: String,
}

impl ServeError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ServeError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<OakError> for ServeError {
    fn from(e: OakError) -> Self {
        ServeError::internal(e.to_string())
    }
}

/// Run a synchronous (rusqlite) repository operation off the async runtime.
/// SQLite calls block, so they must not run on a tokio worker thread.
async fn blocking<T>(
    f: impl FnOnce() -> std::result::Result<T, ServeError> + Send + 'static,
) -> std::result::Result<T, ServeError>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(ServeError::internal(format!("blocking task panicked: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Repo path resolution
// ---------------------------------------------------------------------------

/// Validate a single `{owner}` / `{name}` path segment and reject anything
/// that could escape the data directory (`..`, slashes, etc.).
fn check_segment(seg: &str) -> std::result::Result<(), ServeError> {
    let ok = !seg.is_empty()
        && seg != ".."
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ServeError::bad_request(format!(
            "invalid repo path segment: {seg:?}"
        )))
    }
}

/// `<root>/<owner>/<name>.oakdb`.
fn repo_db_path(root: &Path, owner: &str, name: &str) -> std::result::Result<PathBuf, ServeError> {
    check_segment(owner)?;
    check_segment(name)?;
    Ok(root.join(owner).join(format!("{name}.oakdb")))
}

/// Open an existing repo, or 404 if its file doesn't exist yet.
fn open_existing(
    root: &Path,
    owner: &str,
    name: &str,
) -> std::result::Result<SqliteRepository, ServeError> {
    let path = repo_db_path(root, owner, name)?;
    if !path.exists() {
        return Err(ServeError::not_found(format!(
            "repository '{owner}/{name}' not found"
        )));
    }
    SqliteRepository::open(&path).map_err(ServeError::from)
}

/// Open a repo for writing, creating the file + schema if absent. Foreign-key
/// enforcement is off (like the mount cache) so we can ingest server-shaped
/// data — branches whose parent isn't materialized, commits out of topo order —
/// exactly as the CLI's bulk-import path does.
fn open_for_write(
    root: &Path,
    owner: &str,
    name: &str,
) -> std::result::Result<SqliteRepository, ServeError> {
    let path = repo_db_path(root, owner, name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ServeError::internal(format!("create repo dir: {e}")))?;
    }
    SqliteRepository::open_relaxed(&path).map_err(ServeError::from)
}

// ---------------------------------------------------------------------------
// Default-branch resolution + conversions (mirrors the hosted server/CLI)
// ---------------------------------------------------------------------------

/// Resolve the name of the repo's default branch for clone/pull:
///   1. `main` if it has a head (the post-merge norm);
///   2. else the first root branch (no parent) with a head;
///   3. else *any* branch with a head.
///
/// Step 3 matters for this minimal server: it has no squash-merge-to-`main`
/// endpoint, so a repo that's only ever had a feature branch pushed to it would
/// otherwise be un-cloneable. Falling back to the feature branch makes the
/// plain "push here, clone there" flow work without a merge step.
fn default_branch_name(repo: &SqliteRepository) -> Result<Option<String>> {
    if repo.get_branch_head("main")?.is_some() {
        return Ok(Some("main".to_string()));
    }
    let branches = repo.list_branches()?;
    for b in &branches {
        if b.parent_branch.is_none() && repo.get_branch_head(&b.name)?.is_some() {
            return Ok(Some(b.name.clone()));
        }
    }
    for b in &branches {
        if repo.get_branch_head(&b.name)?.is_some() {
            return Ok(Some(b.name.clone()));
        }
    }
    Ok(None)
}

fn branch_to_pull(b: &Branch) -> BranchPullData {
    BranchPullData {
        name: b.name.clone(),
        description: b.description.clone(),
        parent_branch: b.parent_branch.clone(),
        status: b.status.to_string(),
        created_at: b.created_at.to_rfc3339(),
        close_reason: b.close_reason.as_ref().map(|r| r.as_str().to_string()),
    }
}

fn commit_to_wire(c: &Commit) -> CommitData {
    CommitData {
        hash: c.hash.to_string(),
        branch_name: c.branch_name.clone(),
        parent_hash: c.parent_hash.as_ref().map(|h| h.to_string()),
        merge_parent_hash: c.merge_parent_hash.as_ref().map(|h| h.to_string()),
        manifest_hash: c.manifest_hash.to_string(),
        author: c.author.clone(),
        message: c.message.clone(),
        timestamp: c.timestamp.to_rfc3339(),
        files: c
            .files
            .iter()
            .map(|f| FileChangeData {
                path: f.path.clone(),
                change_type: match f.change_type {
                    ChangeType::Added => "added".to_string(),
                    ChangeType::Modified => "modified".to_string(),
                    ChangeType::Deleted => "deleted".to_string(),
                    ChangeType::Renamed => "renamed".to_string(),
                },
                old_blob_hash: f.old_blob_hash.as_ref().map(|h| h.to_string()),
                new_blob_hash: f.new_blob_hash.as_ref().map(|h| h.to_string()),
                old_path: f.old_path.clone(),
                old_mode: f
                    .old_mode
                    .map(|mode| oak_core::protocol::file_mode_to_wire(mode).to_string()),
                new_mode: f
                    .new_mode
                    .map(|mode| oak_core::protocol::file_mode_to_wire(mode).to_string()),
            })
            .collect(),
    }
}

fn parse_ts(s: &str) -> std::result::Result<chrono::DateTime<chrono::Utc>, ServeError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| ServeError::bad_request(format!("invalid timestamp {s:?}: {e}")))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn capabilities() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "push_protocol": "staged_v1",
        "staged_session_protocol": "opaque_v1",
        "staged_capabilities_ready": true,
        "staged_abort_protocol": oak_core::protocol::STAGED_ABORT_PROTOCOL,
        "mapping_proof_protocol": oak_core::protocol::MAPPING_PROOF_PROTOCOL,
        "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL,
        "known_loss_protocol": oak_core::protocol::KNOWN_LOSS_PROTOCOL
    }))
}

#[derive(Default, Serialize, Deserialize)]
struct PublishedObjectClosure {
    commits: HashSet<String>,
    trees: HashSet<String>,
    blobs: HashSet<String>,
    #[serde(default)]
    chunks: HashSet<String>,
    /// Older cached closures predate chunk visibility tracking. False means
    /// the cache must be upgraded from durable published blob mappings before
    /// serving any hash-addressed chunk request.
    #[serde(default)]
    chunk_closure_complete: bool,
}

fn published_object_closure(repo: &SqliteRepository) -> Result<PublishedObjectClosure> {
    if let Some(encoded) = repo.get_metadata(MetadataKey::ServePublishedClosure)? {
        let mut closure: PublishedObjectClosure =
            serde_json::from_str(&encoded).map_err(|error| {
                OakError::Database(format!("invalid published closure cache: {error}"))
            })?;
        if !closure.chunk_closure_complete {
            let blobs: Vec<String> = closure.blobs.iter().cloned().collect();
            for blob in blobs {
                let hash = Hash::from_hex(&blob)?;
                if let Some(chunks) = repo.get_blob_chunks(&hash)? {
                    closure
                        .chunks
                        .extend(chunks.into_iter().map(|chunk| chunk.hash.to_string()));
                }
            }
            closure.chunk_closure_complete = true;
            store_published_object_closure(repo, &closure)?;
        }
        return Ok(closure);
    }
    let mut closure = PublishedObjectClosure {
        chunk_closure_complete: true,
        ..PublishedObjectClosure::default()
    };
    let mut commit_stack = Vec::new();
    for branch in repo.list_branches()? {
        if let Some(head) = repo.get_branch_head(&branch.name)? {
            commit_stack.push(head);
        }
    }
    let mut tree_stack = Vec::new();
    while let Some(hash) = commit_stack.pop() {
        if !closure.commits.insert(hash.to_string()) {
            continue;
        }
        let commit = repo
            .get_commit(&hash)?
            .ok_or_else(|| OakError::Database(format!("published commit {hash} is missing")))?;
        tree_stack.push(commit.manifest_hash);
        if let Some(parent) = commit.parent_hash {
            commit_stack.push(parent);
        }
        if let Some(merge_parent) = commit.merge_parent_hash {
            commit_stack.push(merge_parent);
        }
    }
    while let Some(hash) = tree_stack.pop() {
        if hash == Tree::empty_hash() || !closure.trees.insert(hash.to_string()) {
            continue;
        }
        let tree = repo
            .get_tree(&hash)?
            .ok_or_else(|| OakError::ManifestNotFound(hash.to_string()))?;
        for entry in tree.entries {
            match entry.kind {
                oak_core::TreeEntryKind::Tree => tree_stack.push(entry.hash),
                oak_core::TreeEntryKind::Blob => {
                    if closure.blobs.insert(entry.hash.to_string()) {
                        if let Some(chunks) = repo.get_blob_chunks(&entry.hash)? {
                            closure
                                .chunks
                                .extend(chunks.into_iter().map(|chunk| chunk.hash.to_string()));
                        }
                    }
                }
            }
        }
    }
    store_published_object_closure(repo, &closure)?;
    Ok(closure)
}

fn store_published_object_closure(
    repo: &SqliteRepository,
    closure: &PublishedObjectClosure,
) -> Result<()> {
    let encoded = serde_json::to_string(closure)
        .map_err(|error| OakError::Database(format!("encode published closure: {error}")))?;
    repo.set_metadata(MetadataKey::ServePublishedClosure, &encoded)
}

fn record_published_objects(
    repo: &SqliteRepository,
    commits: impl IntoIterator<Item = String>,
    trees: impl IntoIterator<Item = String>,
    blobs: impl IntoIterator<Item = String>,
) -> Result<()> {
    let mut closure = published_object_closure(repo)?;
    closure.commits.extend(commits);
    closure.trees.extend(trees);
    let blobs: Vec<String> = blobs.into_iter().collect();
    closure.blobs.extend(blobs.iter().cloned());
    for blob in blobs {
        let hash = Hash::from_hex(&blob)?;
        if let Some(chunks) = repo.get_blob_chunks(&hash)? {
            closure
                .chunks
                .extend(chunks.into_iter().map(|chunk| chunk.hash.to_string()));
        }
    }
    closure.chunk_closure_complete = true;
    store_published_object_closure(repo, &closure)
}

fn verify_stored_blob(repo: &SqliteRepository, hash: &Hash) -> Result<bool> {
    let Some(size) = repo.get_blob_size(hash)? else {
        return Ok(false);
    };
    let mut chunks = repo.get_blob_chunks(hash)?.unwrap_or_default();
    if chunks.is_empty() {
        return Ok(repo.get_blob(hash)?.is_some_and(|blob| {
            blob.size == size
                && blob.content.len() as u64 == size
                && hash_bytes(&blob.content) == *hash
        }));
    }
    chunks.sort_by_key(|chunk| chunk.offset);
    let mut expected_offset = 0u64;
    let mut hasher = blake3::Hasher::new();
    for chunk in chunks {
        if chunk.offset != expected_offset {
            return Ok(false);
        }
        let Some(bytes) = repo.get_chunk(&chunk.hash)? else {
            return Ok(false);
        };
        if bytes.len() != chunk.length as usize || hash_bytes(&bytes) != chunk.hash {
            return Ok(false);
        }
        expected_offset = expected_offset.saturating_add(chunk.length as u64);
        hasher.update(&bytes);
    }
    Ok(expected_offset == size && hasher.finalize().to_hex().as_str() == hash.as_str())
}

fn store_blob_mapping_immutable(
    repo: &SqliteRepository,
    blob_hash: &Hash,
    chunks: &[ChunkInfo],
) -> std::result::Result<(), ServeError> {
    if let Some(existing) = repo.get_blob_chunks(blob_hash)? {
        let exact_replay = existing.len() == chunks.len()
            && existing.iter().zip(chunks).all(|(left, right)| {
                left.hash == right.hash
                    && left.offset == right.offset
                    && left.length == right.length
            });
        if exact_replay {
            return Ok(());
        }
        return Err(ServeError::bad_request(format!(
            "blob {blob_hash} is already bound to a different canonical chunk mapping"
        )));
    }
    repo.store_blob_chunks(blob_hash, chunks)?;
    Ok(())
}

/// `POST /api/repos` — create a repo. `organization_slug` is treated as the
/// owner namespace (no org concept); it defaults to `local`.
async fn create_repo(
    State(state): State<ServeState>,
    Json(req): Json<CreateRepoRequest>,
) -> std::result::Result<Json<RepoResponse>, ServeError> {
    let owner = req
        .organization_slug
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let name = req.name.clone();
    let root = state.root.clone();
    blocking(move || {
        // Opening for write creates the file + schema.
        open_for_write(&root, &owner, &name)?;
        Ok(Json(RepoResponse {
            name,
            description: req.description,
            head: None,
            is_public: req.is_public,
            owner: Some(owner),
            emoji: None,
            updated_at: None,
        }))
    })
    .await
}

async fn commit_info(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<CommitInfoRequest>,
) -> std::result::Result<Json<CommitInfoResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        if req.hashes.len() > 256 {
            return Err(ServeError::bad_request("commit info exceeds 256-hash cap"));
        }
        let mut commits = Vec::new();
        let mut trees = Vec::new();
        let mut seen_trees = HashSet::new();
        let mut seen_commits = HashSet::new();
        let published = published_object_closure(&repo)?;
        for hash in req.hashes {
            let hash = Hash::from_hex(&hash).map_err(|error| {
                ServeError::bad_request(format!("invalid commit hash: {error}"))
            })?;
            if !seen_commits.insert(hash.clone()) {
                return Err(ServeError::bad_request(
                    "commit info contains duplicate hash",
                ));
            }
            if !published.commits.contains(hash.as_str()) {
                continue;
            }
            let Some(commit) = repo.get_commit(&hash)? else {
                continue;
            };
            if !req.metadata_only {
                let mut fetch = |tree_hash: &Hash| -> Result<Tree> {
                    repo.get_tree(tree_hash)?
                        .ok_or_else(|| OakError::ManifestNotFound(tree_hash.to_string()))
                };
                for tree in collect_tree_objects(&commit.manifest_hash, &mut fetch)? {
                    if seen_trees.insert(tree.hash.to_string()) {
                        trees.push(tree_to_wire(&tree));
                    }
                }
            }
            commits.push(commit_to_wire(&commit));
        }
        Ok(Json(CommitInfoResponse { commits, trees }))
    })
    .await
}

/// `GET /api/{owner}/{name}` — repo existence + default-branch head.
async fn get_repo(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
) -> std::result::Result<Json<RepoResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let head = match default_branch_name(&repo)? {
            Some(b) => repo.get_branch_head(&b)?.map(|h| h.to_string()),
            None => None,
        };
        Ok(Json(RepoResponse {
            name,
            description: None,
            head,
            is_public: true,
            owner: Some(owner),
            emoji: None,
            updated_at: None,
        }))
    })
    .await
}

/// `GET /api/{owner}/{name}/branches/{branch}` — the branch head (the client
/// reads only `head`). Unknown branch → `{ "head": null }`.
async fn get_branch(
    State(state): State<ServeState>,
    AxPath((owner, name, branch)): AxPath<(String, String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let head = repo.get_branch_head(&branch)?.map(|h| h.to_string());
        Ok(Json(serde_json::json!({ "head": head })))
    })
    .await
}

/// How many object writes to batch per bulk-transaction flush during a push,
/// bounding WAL growth on a large import without re-introducing per-object
/// fsyncs. Matches the clone/pull ingest's `BULK_FLUSH_ROWS`.
/// `POST /api/{owner}/{name}/push` — ingest commits/blobs/trees onto a branch.
/// No org/quota/`main`-protection logic — just optimistic-concurrency conflict
/// detection and a store.
async fn push(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<PushRequest>,
) -> std::result::Result<Json<PushResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;

        // Optimistic-concurrency check (skipped on --force). Mirrors the hosted
        // server: a per-branch fast-forward/idempotent check when the push
        // carries branch metadata, else the legacy global-head check.
        //
        // A push with no commits at all is exempt: that's the metadata-only
        // shape `push_branch_metadata` sends for `oak desc` — it upserts the
        // branch row and moves no heads, so there is nothing to CAS against.
        // Running it through the head checks below would reject every
        // description-only sync with a bogus "remote head has changed".
        let metadata_only = req.branch.is_some() && req.commits.is_empty();
        if !metadata_only {
            let branch = req.branch.as_ref().ok_or_else(|| {
                ServeError::bad_request("ordinary object push requires branch metadata")
            })?;
            let audit = StagedPushRequest {
                stage_id: "ordinary_push_validation".to_string(),
                expected_branch_head: req.expected_branch_head.clone(),
                branch: branch.clone(),
                finalize: false,
                force: false,
                target_head: None,
                commits: req.commits.clone(),
                blobs: req.blobs.clone(),
                trees: req.trees.clone(),
            };
            let published = published_object_closure(&repo)?;
            let admitted = validate_staged_closure(
                &audit,
                |hash| Ok(published.commits.contains(hash.as_str())),
                |hash| {
                    if published.trees.contains(hash.as_str()) {
                        repo.get_tree(hash).map_err(|error| error.to_string())
                    } else {
                        Ok(None)
                    }
                },
                |hash| {
                    if published.blobs.contains(hash.as_str()) {
                        verify_stored_blob(&repo, hash).map_err(|error| error.to_string())
                    } else {
                        Ok(false)
                    }
                },
                |hash| repo.get_chunk(hash).map_err(|error| error.to_string()),
            )
            .map_err(ServeError::bad_request)?;
            if admitted
                .commits
                .iter()
                .any(|commit| commit.branch_name != branch.name)
            {
                return Err(ServeError::bad_request(
                    "ordinary push commits must belong to the target branch",
                ));
            }
            let incoming: HashSet<String> = admitted
                .commits
                .iter()
                .map(|commit| commit.hash.to_string())
                .collect();
            let referenced: HashSet<String> = admitted
                .commits
                .iter()
                .flat_map(|commit| [&commit.parent_hash, &commit.merge_parent_hash])
                .flatten()
                .filter(|hash| incoming.contains(hash.as_str()))
                .map(ToString::to_string)
                .collect();
            let tips: Vec<&Commit> = admitted
                .commits
                .iter()
                .filter(|commit| !referenced.contains(commit.hash.as_str()))
                .collect();
            if !admitted.commits.is_empty() && tips.len() != 1 {
                return Err(ServeError::bad_request(
                    "ordinary push requires exactly one incoming dominating tip",
                ));
            }
            if let Some(tip) = tips.first() {
                let mut reached = HashSet::new();
                let mut stack = vec![tip.hash.clone()];
                while let Some(hash) = stack.pop() {
                    if !incoming.contains(hash.as_str()) || !reached.insert(hash.to_string()) {
                        continue;
                    }
                    let commit = admitted
                        .commits
                        .iter()
                        .find(|commit| commit.hash == hash)
                        .expect("incoming hash indexed above");
                    if let Some(parent) = &commit.parent_hash {
                        stack.push(parent.clone());
                    }
                    if let Some(merge_parent) = &commit.merge_parent_hash {
                        stack.push(merge_parent.clone());
                    }
                }
                if reached != incoming {
                    return Err(ServeError::bad_request(
                        "ordinary push contains commits not reachable from its exposed tip",
                    ));
                }
            }
        }
        if !req.force && !metadata_only {
            let conflicted = if let Some(branch_data) = &req.branch {
                let pushed: HashSet<&str> = req
                    .commits
                    .iter()
                    .filter(|c| c.branch_name == branch_data.name)
                    .map(|c| c.hash.as_str())
                    .collect();
                let chain_starts: Vec<&CommitData> = req
                    .commits
                    .iter()
                    .filter(|c| {
                        c.branch_name == branch_data.name
                            && match &c.parent_hash {
                                None => true,
                                Some(p) => !pushed.contains(p.as_str()),
                            }
                    })
                    .collect();
                if chain_starts.is_empty() {
                    let expected = req.expected_head.as_ref().map(|s| Hash(s.clone()));
                    repo.get_head()? != expected
                } else {
                    let server_head = repo.get_branch_head(&branch_data.name)?;
                    let server_head_str = server_head.as_ref().map(|h| h.0.as_str());
                    let extending = chain_starts
                        .iter()
                        .any(|c| c.parent_hash.as_deref() == server_head_str);
                    let idempotent = req.commits.iter().any(|c| {
                        c.branch_name == branch_data.name
                            && Some(c.hash.as_str()) == server_head_str
                    });
                    !(server_head_str.is_none() || extending || idempotent)
                }
            } else {
                let expected = req.expected_head.as_ref().map(|s| Hash(s.clone()));
                repo.get_head()? != expected
            };

            if conflicted {
                return Ok(Json(PushResponse {
                    success: false,
                    new_head: repo.get_head()?.map(|h| h.to_string()),
                    message: "Conflict: remote head has changed. Pull first.".to_string(),
                }));
            }
        }

        // Every object write below goes through one relaxed-durability bulk
        // transaction. Outside it, each `store_*` auto-commits, and in WAL mode
        // with `synchronous=FULL` that fsyncs per statement — so a push paid
        // roughly one fsync per commit *plus* one per file/blob/chunk/tree row,
        // which dominated web-import time (~one fsync-bound commit at a time).
        // This mirrors the `oak clone` / `oak pull` ingest. The guard rolls
        // back on drop, so an early `return Err` (e.g. a bad wire object) can't
        // leave a half-applied push behind — previously those committed object
        // by object.
        let bulk = super::BulkTxn::begin(&repo)?;

        // Branch metadata. `store_branch` is insert-if-absent by design
        // (callers all over the CLI rely on it never clobbering an existing
        // row), so for a row that already exists pushed metadata must be
        // applied explicitly. Metadata-only pushes are narrower: `oak desc`
        // must not let a stale open checkout reopen a server-closed branch,
        // while `oak close` still has to close an open one.
        if let Some(bd) = &req.branch {
            let branch = Branch {
                name: bd.name.clone(),
                description: bd.description.clone(),
                parent_branch: bd.parent_branch.clone(),
                status: BranchStatus::from_db_str(&bd.status),
                close_reason: bd
                    .close_reason
                    .as_deref()
                    .map(CloseReason::parse)
                    .transpose()
                    .map_err(|e| ServeError::bad_request(e.to_string()))?,
                created_at: parse_ts(&bd.created_at)?,
            };
            if metadata_only {
                if let Some(current) = repo.get_branch(&bd.name)? {
                    if let Some(desc) = &bd.description {
                        repo.update_branch_description(&bd.name, desc)?;
                    }
                    match (current.status, branch.status) {
                        (BranchStatus::Open, BranchStatus::Closed) => {
                            repo.update_branch_status(&bd.name, BranchStatus::Closed)?;
                            if let Some(reason) = branch.close_reason.clone() {
                                repo.update_branch_close_reason(&bd.name, reason)?;
                            }
                        }
                        (BranchStatus::Closed, BranchStatus::Closed) => {
                            if let Some(reason) = branch.close_reason.clone() {
                                repo.update_branch_close_reason(&bd.name, reason)?;
                            }
                        }
                        (BranchStatus::Closed, BranchStatus::Open)
                        | (BranchStatus::Open, BranchStatus::Open) => {}
                    }
                } else {
                    repo.store_branch(&branch)?;
                    if let Some(desc) = &bd.description {
                        repo.update_branch_description(&bd.name, desc)?;
                    }
                    repo.update_branch_status(&bd.name, branch.status)?;
                    if let Some(reason) = branch.close_reason.clone() {
                        repo.update_branch_close_reason(&bd.name, reason)?;
                    }
                }
            } else {
                repo.store_branch(&branch)?;
                if let Some(desc) = &bd.description {
                    repo.update_branch_description(&bd.name, desc)?;
                }
                repo.update_branch_status(&bd.name, BranchStatus::from_db_str(&bd.status))?;
                if let Some(reason) = branch.close_reason.clone() {
                    repo.update_branch_close_reason(&bd.name, reason)?;
                }
            }
        }

        // Tree objects.
        for td in &req.trees {
            let tree = tree_data_to_core(td).map_err(ServeError::bad_request)?;
            repo.store_tree(&tree)?;
        }

        // Blobs. The client's download path (pull *and* clone) always expects
        // blobs to come back as chunk refs — there is no inline-content wire
        // format on read. So every blob lands as a `blob_chunks` mapping, just
        // like the hosted server:
        //   * inline blobs (bytes on the wire) → one self-chunk keyed by the
        //     blob hash;
        //   * chunked blobs → their chunks were uploaded ahead of this request
        //     via `PUT /chunks/{hash}`; we record the map and reassemble the
        //     `blobs` row (what `has_blob` uses for dedup on the next push).
        for b in &req.blobs {
            // Wire hashes are untrusted: a non-hex "hash" would poison the
            // content-addressed tables (and anything that later embeds the
            // string in a hash preimage), so reject it at the door.
            let blob_hash =
                Hash::from_hex(&b.hash).map_err(|e| ServeError::bad_request(e.to_string()))?;
            if b.chunks.is_empty() {
                let length = u32::try_from(b.size).map_err(|_| {
                    ServeError::bad_request(format!(
                        "inline blob {} too large for single-chunk encoding ({} bytes)",
                        b.hash, b.size
                    ))
                })?;
                // Content is untrusted too: a blob stored under a hash it
                // doesn't match poisons every future pull of this repo.
                if oak_core::hash_bytes(&b.content) != blob_hash {
                    return Err(ServeError::bad_request(format!(
                        "inline blob {} does not hash to its claimed hash",
                        b.hash
                    )));
                }
                repo.store_chunk(&blob_hash, &b.content)?;
                repo.store_blob(&Blob {
                    hash: blob_hash.clone(),
                    content: b.content.clone(),
                    size: b.size,
                })?;
                store_blob_mapping_immutable(
                    &repo,
                    &blob_hash,
                    &[ChunkInfo {
                        hash: blob_hash.clone(),
                        offset: 0,
                        length,
                    }],
                )?;
            } else {
                let mut refs = b.chunks.clone();
                refs.sort_by_key(|c| c.offset);
                let mut chunk_infos: Vec<ChunkInfo> = Vec::with_capacity(refs.len());
                let mut expected_offset = 0u64;
                let mut hasher = blake3::Hasher::new();
                for c in &refs {
                    if c.offset != expected_offset {
                        return Err(ServeError::bad_request(format!(
                            "blob {} has non-contiguous chunk mapping",
                            b.hash
                        )));
                    }
                    let chunk_hash = Hash::from_hex(&c.hash)
                        .map_err(|e| ServeError::bad_request(e.to_string()))?;
                    let content = repo.get_chunk(&chunk_hash)?.ok_or_else(|| {
                        ServeError::bad_request(format!(
                            "chunk {} for blob {} was not uploaded before push",
                            c.hash, b.hash
                        ))
                    })?;
                    if content.len() != c.size as usize || hash_bytes(&content) != chunk_hash {
                        return Err(ServeError::bad_request(format!(
                            "chunk {} for blob {} fails physical verification",
                            c.hash, b.hash
                        )));
                    }
                    hasher.update(&content);
                    expected_offset = expected_offset.saturating_add(c.size as u64);
                    chunk_infos.push(ChunkInfo {
                        hash: chunk_hash,
                        offset: c.offset,
                        length: c.size,
                    });
                }
                if expected_offset != b.size
                    || hasher.finalize().to_hex().as_str() != blob_hash.as_str()
                {
                    return Err(ServeError::bad_request(format!(
                        "blob {} streamed from its chunks does not match its declared size/hash",
                        b.hash
                    )));
                }
                repo.store_blob(&Blob {
                    hash: blob_hash.clone(),
                    content: Vec::new(),
                    size: b.size,
                })?;
                store_blob_mapping_immutable(&repo, &blob_hash, &chunk_infos)?;
            }
        }

        // Commits + branch heads. Commits arrive oldest-first, so the last one
        // seen per branch is that branch's new head.
        let mut heads: HashMap<String, Hash> = HashMap::new();
        for c in &req.commits {
            let commit =
                oak_core::protocol::commit_data_to_core(c).map_err(ServeError::bad_request)?;
            repo.store_commit(&commit)?;
            heads.insert(commit.branch_name.clone(), commit.hash.clone());
        }
        for c in &req.commits {
            let commit =
                oak_core::protocol::commit_data_to_core(c).map_err(ServeError::bad_request)?;
            super::push::admit_commit_file_changes(&repo, &commit)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
        }
        for (branch, head) in &heads {
            repo.set_branch_head(branch, head)?;
        }
        if !heads.is_empty() {
            record_published_objects(
                &repo,
                req.commits.iter().map(|commit| commit.hash.clone()),
                req.trees.iter().map(|tree| tree.hash.clone()),
                req.blobs.iter().map(|blob| blob.hash.clone()),
            )?;
        }

        // Commit the batch and restore full durability before the post-write
        // reads below. On any early return above, the guard's `Drop` rolls back.
        bulk.commit()?;

        let target = req
            .branch
            .as_ref()
            .map(|b| b.name.clone())
            .or_else(|| req.commits.last().map(|c| c.branch_name.clone()));
        let new_head = match target.as_ref().and_then(|t| heads.get(t).cloned()) {
            Some(h) => Some(h),
            None => repo.get_head()?,
        };

        Ok(Json(PushResponse {
            success: true,
            new_head: new_head.map(|h| h.to_string()),
            message: "ok".to_string(),
        }))
    })
    .await
}

/// Versioned, fail-closed publication session. Immutable objects are admitted
/// under one opaque `stage_id`; only its final request may atomically move the
/// exact branch boundary observed by the planner.
async fn staged_push_v1(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<StagedPushRequest>,
) -> std::result::Result<Json<PushResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        oak_core::protocol::validate_stage_id(&req.stage_id).map_err(ServeError::bad_request)?;
        let expected = req
            .expected_branch_head
            .as_deref()
            .map(Hash::from_hex)
            .transpose()
            .map_err(|error| ServeError::bad_request(error.to_string()))?;
        // Repository creation is a separate, explicit mutation performed by
        // `/api/repos` only after the client has admitted its whole local
        // operation. A malformed staged request must never materialize a DB.
        let repo = open_existing(&root, &owner, &name)?;
        let mut sessions = load_active_stage_sessions(&repo)?;
        let request_branch_identity = staged_branch_identity(&req.branch)?;
        if let Some(session) = sessions.get(&req.stage_id) {
            if session.branch != req.branch.name
                || session.branch_identity != request_branch_identity
                || session.expected_head != req.expected_branch_head
                || session.force != req.force
            {
                return Err(ServeError::bad_request(
                    "stage_id is already bound to a different branch boundary",
                ));
            }
            if session.state == ServeStageState::Completed {
                let exact_replay = req.finalize
                    && req.commits.is_empty()
                    && req.trees.is_empty()
                    && req.blobs.is_empty()
                    && req.target_head == session.completed_target;
                if exact_replay {
                    return Ok(Json(PushResponse {
                        success: true,
                        new_head: session.completed_target.clone(),
                        message: "already finalized".to_string(),
                    }));
                }
                return Err(ServeError::bad_request(
                    "completed stage_id accepts only an exact finalization replay",
                ));
            }
            if session.state == ServeStageState::Aborted {
                return Err(ServeError::new(
                    StatusCode::CONFLICT,
                    "staged session was aborted and cannot be reused",
                ));
            }
            if session.state == ServeStageState::Finalizing {
                return Err(ServeError::new(
                    StatusCode::CONFLICT,
                    "staged session is finalizing; retry the exact finalizer",
                ));
            }
        } else if sessions
            .values()
            .filter(|session| !stage_state_is_terminal(session.state))
            .count()
            >= oak_core::protocol::STAGED_MAX_ACTIVE_SESSIONS_PER_REPO
        {
            return Err(ServeError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "repository has reached its active staged-session limit",
            ));
        }
        let current = repo.get_branch_head(&req.branch.name)?;
        if current != expected {
            return Ok(Json(PushResponse {
                success: false,
                new_head: current.map(|hash| hash.to_string()),
                message: "Conflict: staged session boundary changed".to_string(),
            }));
        }
        if req.finalize {
            if !req.commits.is_empty() || !req.trees.is_empty() || !req.blobs.is_empty() {
                return Err(ServeError::bad_request(
                    "staged-v1 finalization requires an empty object envelope",
                ));
            }
            let target = req
                .target_head
                .as_deref()
                .ok_or_else(|| ServeError::bad_request("finalization requires target_head"))
                .and_then(|hash| {
                    Hash::from_hex(hash).map_err(|error| ServeError::bad_request(error.to_string()))
                })?;
            let target_commit = repo.get_commit(&target)?.ok_or_else(|| {
                ServeError::bad_request(format!("target head {target} is not staged"))
            })?;
            let session = sessions
                .get(&req.stage_id)
                .cloned()
                .ok_or_else(|| ServeError::bad_request("staged session is missing or expired"))?;
            if target_commit.branch_name != req.branch.name
                || !session.commits.contains(target.as_str())
            {
                return Err(ServeError::bad_request(
                    "target head does not belong to this staged session and branch",
                ));
            }
            let mut reachable = HashSet::new();
            let mut stack = vec![target.clone()];
            let mut reached_boundary = expected.is_none();
            while let Some(hash) = stack.pop() {
                if expected.as_ref() == Some(&hash) {
                    reached_boundary = true;
                    continue;
                }
                if !reachable.insert(hash.to_string()) {
                    continue;
                }
                let commit = repo.get_commit(&hash)?.ok_or_else(|| {
                    ServeError::bad_request(format!("staged target has missing ancestor {hash}"))
                })?;
                if let Some(parent) = commit.parent_hash {
                    stack.push(parent);
                }
                if let Some(merge_parent) = commit.merge_parent_hash {
                    stack.push(merge_parent);
                }
            }
            if (!req.force && !reached_boundary) || !session.commits.is_subset(&reachable) {
                return Err(ServeError::bad_request(
                    "target head does not reach the expected boundary and every session commit",
                ));
            }

            // Rebuild and verify the complete session commit closure from
            // durable storage immediately before the CAS. This catches a
            // missing/corrupt tree, blob, or chunk after its earlier stage.
            let mut audit_commits = Vec::with_capacity(session.commits.len());
            for hash in &session.commits {
                let hash = Hash::from_hex(hash)
                    .map_err(|error| ServeError::bad_request(error.to_string()))?;
                let commit = repo.get_commit(&hash)?.ok_or_else(|| {
                    ServeError::bad_request(format!("staged commit {hash} is missing"))
                })?;
                audit_commits.push(commit_to_wire(&commit));
            }
            let published = published_object_closure(&repo)?;
            let audit = StagedPushRequest {
                stage_id: req.stage_id.clone(),
                expected_branch_head: req.expected_branch_head.clone(),
                branch: req.branch.clone(),
                finalize: false,
                force: req.force,
                target_head: None,
                commits: audit_commits,
                blobs: Vec::new(),
                trees: Vec::new(),
            };
            let final_audit = validate_staged_closure(
                &audit,
                |hash| {
                    Ok(session.commits.contains(hash.as_str())
                        || published.commits.contains(hash.as_str()))
                },
                |hash| {
                    if session.trees.contains(hash.as_str())
                        || published.trees.contains(hash.as_str())
                    {
                        repo.get_tree(hash).map_err(|error| error.to_string())
                    } else {
                        Ok(None)
                    }
                },
                |hash| {
                    if session.blobs.contains(hash.as_str())
                        || published.blobs.contains(hash.as_str())
                    {
                        verify_stored_blob(&repo, hash).map_err(|error| error.to_string())
                    } else {
                        Ok(false)
                    }
                },
                |hash| repo.get_chunk(hash).map_err(|error| error.to_string()),
            )
            .map_err(ServeError::bad_request)?;
            if final_audit.resolved_tree_entries > MAX_DIRECT_TREE_ENTRIES {
                return Err(ServeError::bad_request(
                    "final staged manifest exceeds admission cap",
                ));
            }

            // Object-only envelopes make bounded staging possible, but they
            // must not become an addressable orphan side channel merely
            // because some unrelated target commits successfully. Prove that
            // every receipt owned by this session is reachable from at least
            // one of its commits before publishing any receipt or branch ref.
            let mut reachable_trees = HashSet::new();
            let mut reachable_blobs = HashSet::new();
            let mut reachable_chunks = HashSet::new();
            let mut tree_stack: Vec<Hash> = final_audit
                .commits
                .iter()
                .map(|commit| commit.manifest_hash.clone())
                .collect();
            while let Some(hash) = tree_stack.pop() {
                if hash == Tree::empty_hash() || !reachable_trees.insert(hash.to_string()) {
                    continue;
                }
                if !session.trees.contains(hash.as_str())
                    && !published.trees.contains(hash.as_str())
                {
                    return Err(ServeError::bad_request(format!(
                        "final staged closure references unowned tree {hash}"
                    )));
                }
                let tree = repo.get_tree(&hash)?.ok_or_else(|| {
                    ServeError::bad_request(format!(
                        "final staged closure is missing reachable tree {hash}"
                    ))
                })?;
                for entry in tree.entries {
                    match entry.kind {
                        oak_core::TreeEntryKind::Tree => tree_stack.push(entry.hash),
                        oak_core::TreeEntryKind::Blob => {
                            if reachable_blobs.insert(entry.hash.to_string()) {
                                match repo.get_blob_chunks(&entry.hash)? {
                                    Some(chunks) => reachable_chunks.extend(
                                        chunks.into_iter().map(|chunk| chunk.hash.to_string()),
                                    ),
                                    None if session.blobs.contains(entry.hash.as_str()) => {
                                        return Err(ServeError::bad_request(format!(
                                            "final staged closure is missing blob mapping {}",
                                            entry.hash
                                        )));
                                    }
                                    None => {}
                                }
                            }
                        }
                    }
                }
            }
            let orphan = session
                .trees
                .iter()
                .find(|hash| !reachable_trees.contains(*hash))
                .map(|hash| ("tree", hash))
                .or_else(|| {
                    session
                        .blobs
                        .iter()
                        .find(|hash| !reachable_blobs.contains(*hash))
                        .map(|hash| ("blob", hash))
                })
                .or_else(|| {
                    session
                        .chunks
                        .iter()
                        .find(|hash| !reachable_chunks.contains(*hash))
                        .map(|hash| ("chunk", hash))
                });
            if let Some((kind, hash)) = orphan {
                return Err(ServeError::bad_request(format!(
                    "staged session contains unreachable {kind} {hash}"
                )));
            }

            let branch = Branch {
                name: req.branch.name.clone(),
                description: req.branch.description.clone(),
                parent_branch: req.branch.parent_branch.clone(),
                status: BranchStatus::from_db_str(&req.branch.status),
                close_reason: req
                    .branch
                    .close_reason
                    .as_deref()
                    .map(CloseReason::parse)
                    .transpose()
                    .map_err(|error| ServeError::bad_request(error.to_string()))?,
                created_at: parse_ts(&req.branch.created_at)?,
            };
            let bulk = super::BulkTxn::begin(&repo)?;
            let current = repo.get_branch_head(&req.branch.name)?;
            if current != expected {
                return Ok(Json(PushResponse {
                    success: false,
                    new_head: current.map(|hash| hash.to_string()),
                    message: "Conflict: staged session boundary changed".to_string(),
                }));
            }
            sessions = load_active_stage_sessions(&repo)?;
            if sessions.get(&req.stage_id).map(|entry| &entry.commits) != Some(&session.commits) {
                return Err(ServeError::bad_request(
                    "staged session changed or expired during finalization",
                ));
            }
            sessions
                .get_mut(&req.stage_id)
                .expect("session checked above")
                .state = ServeStageState::Finalizing;
            repo.store_branch(&branch)?;
            if let Some(description) = &branch.description {
                repo.update_branch_description(&branch.name, description)?;
            }
            repo.set_branch_head(&branch.name, &target)?;
            record_published_objects(
                &repo,
                session.commits.iter().cloned(),
                session.trees.iter().cloned(),
                session.blobs.iter().cloned(),
            )?;
            let completed = sessions
                .get_mut(&req.stage_id)
                .expect("session retained through finalization");
            // A completed record is only an idempotency tombstone. Retaining
            // the staged closure here makes every future request parse an
            // operation-sized receipt forever and duplicates immutable-store
            // ownership that no longer exists. Exact replay identity remains
            // bound by stage_id + branch_identity + expected boundary + target.
            compact_completed_stage_session(
                completed,
                target.to_string(),
                chrono::Utc::now().timestamp(),
            );
            prune_completed_stage_tombstones(&mut sessions);
            store_stage_sessions(&repo, &sessions)?;
            bulk.commit()?;
            return Ok(Json(PushResponse {
                success: true,
                new_head: Some(target.to_string()),
                message: "ok".to_string(),
            }));
        }

        validate_object_envelope_caps(
            &req.commits,
            &req.trees,
            &req.blobs,
            &req.branch,
            oak_core::protocol::STAGED_ENVELOPE_MAX_COMMITS,
        )?;
        let published = published_object_closure(&repo)?;
        validate_staged_mapping_proof_tokens(&owner, &name, &req.blobs, &published, &repo)?;
        let session = sessions.get(&req.stage_id).cloned().unwrap_or_default();
        let admitted = validate_staged_closure(
            &req,
            |hash| {
                Ok(session.commits.contains(hash.as_str())
                    || published.commits.contains(hash.as_str()))
            },
            |hash| {
                if session.trees.contains(hash.as_str()) || published.trees.contains(hash.as_str())
                {
                    repo.get_tree(hash).map_err(|error| error.to_string())
                } else {
                    Ok(None)
                }
            },
            |hash| {
                if session.blobs.contains(hash.as_str()) || published.blobs.contains(hash.as_str())
                {
                    verify_stored_blob(&repo, hash).map_err(|error| error.to_string())
                } else {
                    Ok(false)
                }
            },
            |hash| repo.get_chunk(hash).map_err(|error| error.to_string()),
        )
        .map_err(ServeError::bad_request)?;
        if admitted.resolved_tree_entries > MAX_DIRECT_TREE_ENTRIES {
            return Err(ServeError::bad_request(
                "staged-v1 resolved manifest exceeds admission cap",
            ));
        }
        if admitted
            .commits
            .iter()
            .any(|commit| commit.branch_name != req.branch.name)
        {
            return Err(ServeError::bad_request(
                "staged commits must belong to the target branch",
            ));
        }
        let updated_session =
            updated_stage_session(&repo, &req, &admitted, sessions.get(&req.stage_id))?;
        let prior_session = sessions.get(&req.stage_id).cloned();

        let bulk = super::BulkTxn::begin(&repo)?;
        let current = repo.get_branch_head(&req.branch.name)?;
        if current != expected {
            return Ok(Json(PushResponse {
                success: false,
                new_head: current.map(|hash| hash.to_string()),
                message: "Conflict: staged session boundary changed".to_string(),
            }));
        }
        for tree in &admitted.trees {
            repo.store_tree(tree)?;
        }
        for blob in &admitted.blobs {
            if let Some((hash, content)) = &blob.inline_chunk {
                repo.store_chunk(hash, content)?;
            }
            repo.store_blob(&blob.blob)?;
            store_blob_mapping_immutable(&repo, &blob.blob.hash, &blob.chunks)?;
        }
        for commit in &admitted.commits {
            repo.store_commit(commit)?;
        }
        for commit in &admitted.commits {
            super::push::admit_commit_file_changes(&repo, commit)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
        }
        sessions = load_active_stage_sessions(&repo)?;
        if sessions.get(&req.stage_id) != prior_session.as_ref() {
            return Err(ServeError::new(
                StatusCode::CONFLICT,
                "staged session changed concurrently; retry the same request",
            ));
        }
        sessions.insert(req.stage_id.clone(), updated_session);
        store_stage_sessions(&repo, &sessions)?;
        bulk.commit()?;
        Ok(Json(PushResponse {
            success: true,
            new_head: None,
            message: "staged".to_string(),
        }))
    })
    .await
}

/// Release an unfinished staged session without publishing any of its object
/// receipts. SQLite's write transaction serializes this with finalization: an
/// abort that wins leaves a durable terminal tombstone, while a completed
/// finalization wins atomically and makes abort return 409.
async fn abort_staged_push_v1(
    State(state): State<ServeState>,
    AxPath((owner, name, stage_id)): AxPath<(String, String, String)>,
    Json(req): Json<StagedAbortRequest>,
) -> std::result::Result<Json<StagedAbortResponse>, ServeError> {
    oak_core::protocol::validate_stage_id(&stage_id).map_err(ServeError::bad_request)?;
    req.expected_branch_head
        .as_deref()
        .map(Hash::from_hex)
        .transpose()
        .map_err(|error| ServeError::bad_request(error.to_string()))?;
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let bulk = super::BulkTxn::begin(&repo)?;
        let mut sessions = load_active_stage_sessions(&repo)?;
        let Some(session) = sessions.get_mut(&stage_id) else {
            bulk.commit()?;
            return Ok(Json(StagedAbortResponse {
                aborted: false,
                state: "missing".to_string(),
            }));
        };
        if session.branch != req.branch_name || session.expected_head != req.expected_branch_head {
            return Err(ServeError::bad_request(
                "staged abort does not match the session branch boundary",
            ));
        }
        match session.state {
            ServeStageState::Completed => {
                return Err(ServeError::new(
                    StatusCode::CONFLICT,
                    "completed staged session cannot be aborted",
                ));
            }
            ServeStageState::Aborted => {}
            ServeStageState::Active | ServeStageState::Finalizing => {
                compact_aborted_stage_session(session, chrono::Utc::now().timestamp());
                prune_completed_stage_tombstones(&mut sessions);
                store_stage_sessions(&repo, &sessions)?;
            }
        }
        bulk.commit()?;
        Ok(Json(StagedAbortResponse {
            aborted: true,
            state: oak_core::protocol::STAGED_ABORTED_STATE.to_string(),
        }))
    })
    .await
}

/// `GET /api/{owner}/{name}/pull` — return commits (+ reachable trees/blobs and
/// branch heads) on the requested branch since `?since=`. Clone (no
/// `branch_name`) defaults to the repo's default branch.
async fn pull(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Query(query): Query<PullQuery>,
) -> std::result::Result<Json<PullResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;

        let target = match &query.branch_name {
            Some(b) => Some(b.clone()),
            None => default_branch_name(&repo)?,
        };

        let since = query.since.as_ref().map(|s| Hash(s.clone()));
        let published = published_object_closure(&repo)?;
        let mut commits = match &target {
            Some(b) => repo.get_commits_since(b, since.as_ref())?,
            None => Vec::new(),
        };
        commits.retain(|commit| published.commits.contains(commit.hash.as_str()));

        // Collect every tree object and blob reachable from the returned
        // commits' root trees.
        let mut seen_trees: HashSet<String> = HashSet::new();
        let mut trees = Vec::new();
        let mut seen_blobs: HashSet<String> = HashSet::new();
        let mut blobs = Vec::new();
        let empty = Tree::empty_hash();
        for c in &commits {
            let root_tree = &c.manifest_hash;
            if root_tree == &empty {
                continue;
            }
            let mut fetch = |h: &Hash| {
                repo.get_tree(h)?
                    .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
            };
            for t in collect_tree_objects(root_tree, &mut fetch)? {
                if seen_trees.insert(t.hash.0.clone()) {
                    trees.push(tree_to_wire(&t));
                }
            }
            for entry in repo.walk_tree(root_tree)? {
                if !seen_blobs.insert(entry.blob_hash.0.clone()) {
                    continue;
                }
                match repo.get_blob_chunks(&entry.blob_hash)? {
                    Some(chunks) if !chunks.is_empty() => {
                        let size = chunks.iter().map(|c| c.length as u64).sum();
                        blobs.push(BlobData {
                            hash: entry.blob_hash.to_string(),
                            content: Vec::new(),
                            size,
                            chunks: chunks
                                .iter()
                                .map(|c| ChunkRefData {
                                    hash: c.hash.to_string(),
                                    offset: c.offset,
                                    size: c.length,
                                })
                                .collect(),
                            mapping_proof_token: None,
                        });
                    }
                    _ => {
                        if let Some(blob) = repo.get_blob(&entry.blob_hash)? {
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

        let all_branches = repo.list_branches()?;
        let branches: Vec<BranchPullData> = all_branches.iter().map(branch_to_pull).collect();
        let (branch, head) = match &target {
            Some(b) => {
                let bp = all_branches
                    .iter()
                    .find(|x| &x.name == b)
                    .map(branch_to_pull);
                (bp, repo.get_branch_head(b)?.map(|h| h.to_string()))
            }
            None => (None, None),
        };
        let commits_wire: Vec<CommitData> = commits.iter().map(commit_to_wire).collect();

        Ok(Json(PullResponse {
            head,
            branch,
            branches,
            commits: commits_wire,
            blobs,
            trees,
            renames: Vec::new(),
            // `oak serve` has no path permissions — nothing is ever withheld.
            restricted_blobs: Vec::new(),
            // Local Serve never operator-adjudicates content loss.
            missing_content: Vec::new(),
        }))
    })
    .await
}

/// `POST /api/{owner}/{name}/blobs/check` — which of `hashes` the server lacks.
async fn check_blobs(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<BlobCheckRequest>,
) -> std::result::Result<Json<BlobCheckResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let published = published_object_closure(&repo)?;
        if req.hashes.len() > 10_000 || (req.verify_content && req.hashes.len() > 32) {
            return Err(ServeError::bad_request("blob check exceeds admission cap"));
        }
        let mut unique = HashSet::with_capacity(req.hashes.len());
        for value in &req.hashes {
            let hash = Hash::from_hex(value)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
            if !unique.insert(hash) {
                return Err(ServeError::bad_request(
                    "blob check contains duplicate hash",
                ));
            }
        }
        let mut missing = Vec::new();
        for h in &req.hashes {
            let hash =
                Hash::from_hex(h).map_err(|error| ServeError::bad_request(error.to_string()))?;
            let present = if !published.blobs.contains(hash.as_str()) {
                false
            } else if req.verify_content || req.require_verified_receipts {
                verify_stored_blob(&repo, &hash)?
            } else {
                repo.has_blob(&hash)?
            };
            if !present {
                missing.push(h.clone());
            }
        }
        Ok(Json(BlobCheckResponse {
            missing,
            verified_content: req.verify_content,
            verified_receipts_required: req.require_verified_receipts,
        }))
    })
    .await
}

async fn create_blob_mapping_proof_async_v1(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<BlobProofRequest>,
) -> std::result::Result<(StatusCode, Json<BlobProofResponse>), ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let total_refs = req.blobs.iter().try_fold(0usize, |total, blob| {
            total.checked_add(blob.total_chunks as usize)
        });
        let total_bytes = req
            .blobs
            .iter()
            .fold(0u64, |total, blob| total.saturating_add(blob.size));
        if req.blobs.is_empty()
            || req.blobs.len() > oak_core::protocol::MAPPING_PROOF_MAX_BLOBS
            || total_refs
                .is_none_or(|refs| refs > oak_core::protocol::MAPPING_PROOF_MAX_SET_CHUNK_REFS)
            || total_bytes > oak_core::protocol::MAPPING_PROOF_MAX_SET_BYTES
        {
            return Err(ServeError::bad_request("async-v1 proof set exceeds bounds"));
        }
        let repo = open_existing(&root, &owner, &name)?;
        let published = published_object_closure(&repo)?;
        let mut unique_blobs = HashSet::with_capacity(req.blobs.len());
        for descriptor in &req.blobs {
            let blob_hash = Hash::from_hex(&descriptor.hash)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
            Hash::from_hex(&descriptor.mapping_digest)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
            if !unique_blobs.insert(blob_hash.clone())
                || descriptor.total_chunks == 0
                || descriptor.total_chunks as usize
                    > oak_core::protocol::MAPPING_PROOF_MAX_BLOB_CHUNK_REFS
                || descriptor.size > oak_core::protocol::MAPPING_PROOF_MAX_BLOB_BYTES
            {
                return Err(ServeError::bad_request(
                    "async-v1 proof header has duplicate/empty/out-of-range blob",
                ));
            }
        }
        let request_digest =
            serve_mapping_proof_request_digest(&owner, &name, &req.blobs, &published, &repo)?;
        if let Some(mut existing) =
            repo.find_serve_mapping_proof_by_request_digest(&request_digest)?
        {
            let now = chrono::Utc::now().timestamp();
            if existing.status == "complete"
                && !existing.missing.is_empty()
                && missing_mapping_proof_content_is_now_valid(&repo, &existing)?
                && repo.restart_missing_serve_mapping_proof(&existing.token, now)?
            {
                existing = repo
                    .load_serve_mapping_proof_header(&existing.token)?
                    .ok_or_else(|| {
                        OakError::Database("restarted mapping proof disappeared".into())
                    })?;
            } else if existing.status == "uploading" {
                repo.touch_uploading_serve_mapping_proof(&existing.token, now)?;
                existing.updated_at = now;
            }
            let status = mapping_proof_http_status(&repo, &existing)?;
            return Ok((status, Json(mapping_proof_response(&existing))));
        }
        let mut base_mapping_digests = Vec::with_capacity(req.blobs.len());
        for descriptor in &req.blobs {
            let blob_hash = Hash::from_hex(&descriptor.hash)
                .map_err(|error| ServeError::bad_request(error.to_string()))?;
            if let Some(existing) = repo.get_blob_chunks(&blob_hash)? {
                let existing_digest = canonical_blob_mapping_digest(&existing);
                if existing_digest != descriptor.mapping_digest {
                    return Err(ServeError::new(
                        StatusCode::CONFLICT,
                        "blob already has a different immutable mapping identity",
                    ));
                }
            }
            base_mapping_digests.push(if published.blobs.contains(&descriptor.hash) {
                let base = canonical_blob_mapping_digest(
                    &repo.get_blob_chunks(&blob_hash)?.unwrap_or_default(),
                );
                if base != descriptor.mapping_digest {
                    return Err(ServeError::new(
                        StatusCode::CONFLICT,
                        "published blob is bound to a different mapping generation",
                    ));
                }
                Some(base)
            } else {
                None
            });
        }
        let now = chrono::Utc::now().timestamp();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let job = ServeMappingProofJob {
            token: token.clone(),
            request_digest,
            status: "uploading".to_string(),
            worker_token: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
            mappings: vec![BTreeMap::new(); req.blobs.len()],
            descriptors: req.blobs,
            base_mapping_digests,
            verified: Vec::new(),
            missing: Vec::new(),
        };
        let stored_token = repo.create_serve_mapping_proof(&job).map_err(|error| {
            if error.to_string().contains("too many active mapping proofs") {
                ServeError::new(StatusCode::TOO_MANY_REQUESTS, error.to_string())
            } else {
                ServeError::from(error)
            }
        })?;
        if stored_token == token {
            Ok((StatusCode::ACCEPTED, Json(mapping_proof_response(&job))))
        } else {
            let stored = repo
                .load_serve_mapping_proof_header(&stored_token)?
                .ok_or_else(|| OakError::Database("coalesced mapping proof disappeared".into()))?;
            let status = mapping_proof_http_status(&repo, &stored)?;
            Ok((status, Json(mapping_proof_response(&stored))))
        }
    })
    .await
}

async fn upload_blob_mapping_pages(
    State(state): State<ServeState>,
    AxPath((owner, name, token)): AxPath<(String, String, String)>,
    Json(req): Json<BlobProofPagesRequest>,
) -> std::result::Result<Json<BlobProofPagesResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let accepted_chunks = req
            .pages
            .iter()
            .map(|page| page.chunks.len())
            .sum::<usize>();
        if req.pages.is_empty()
            || accepted_chunks == 0
            || accepted_chunks > oak_core::protocol::MAPPING_PROOF_PAGE_CHUNK_REFS
        {
            return Err(ServeError::bad_request("mapping proof page exceeds bounds"));
        }
        let repo = open_existing(&root, &owner, &name)?;
        let job = repo
            .load_serve_mapping_proof_header(&token)?
            .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
        let terminal = job.status == "complete";
        if !terminal && job.status != "uploading" {
            return Err(ServeError::new(
                StatusCode::CONFLICT,
                "mapping proof is not uploading",
            ));
        }
        for page in &req.pages {
            let blob_index = page.blob_index as usize;
            let descriptor = job.descriptors.get(blob_index).ok_or_else(|| {
                ServeError::bad_request("mapping page blob index is out of range")
            })?;
            let end = page
                .first_chunk_index
                .checked_add(page.chunks.len() as u32)
                .ok_or_else(|| ServeError::bad_request("mapping page index overflow"))?;
            if end > descriptor.total_chunks || page.chunks.is_empty() {
                return Err(ServeError::bad_request(
                    "mapping page exceeds declared chunk count",
                ));
            }
            for chunk in &page.chunks {
                Hash::from_hex(&chunk.hash)
                    .map_err(|error| ServeError::bad_request(error.to_string()))?;
                if chunk.size == 0
                    || chunk.size as u64 > oak_core::protocol::MAPPING_PROOF_MAX_CHUNK_BYTES
                {
                    return Err(ServeError::bad_request(
                        "mapping page chunk size is invalid",
                    ));
                }
            }
        }
        repo.store_serve_mapping_proof_pages(&token, &req.pages, chrono::Utc::now().timestamp())
            .map_err(|error| ServeError::new(StatusCode::CONFLICT, error.to_string()))?;
        let complete_blobs = repo.serve_mapping_proof_complete_blobs(&token)?;
        let all_mappings_complete = complete_blobs.len() == job.descriptors.len();
        Ok(Json(BlobProofPagesResponse {
            accepted_chunks: accepted_chunks as u32,
            complete_blobs,
            all_mappings_complete,
        }))
    })
    .await
}

const MAPPING_PROOF_LEASE_SECS: i64 = 120;
const MAPPING_PROOF_HEARTBEAT_SECS: u64 = 10;

fn run_blob_mapping_proof_worker(
    root: PathBuf,
    owner: String,
    name: String,
    token: String,
    worker_token: String,
) -> std::result::Result<(), ServeError> {
    let repo = open_existing(&root, &owner, &name)?;
    let published = published_object_closure(&repo)?;
    let job = repo
        .load_serve_mapping_proof_header(&token)?
        .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
    if job.status != "running" || job.worker_token.as_deref() != Some(worker_token.as_str()) {
        return Ok(());
    }
    let mut missing = Vec::new();
    let mut next_heartbeat = std::time::Instant::now();
    for (index, descriptor) in job.descriptors.iter().enumerate() {
        let mut hasher = blake3::Hasher::new();
        let mut content_present = true;
        repo.visit_serve_mapping_proof_chunks(&token, index as u32, |_, chunk| {
            if std::time::Instant::now() >= next_heartbeat {
                let now = chrono::Utc::now().timestamp();
                if !repo.heartbeat_serve_mapping_proof(
                    &token,
                    &worker_token,
                    now,
                    now + MAPPING_PROOF_LEASE_SECS,
                )? {
                    return Err(OakError::Database(
                        "mapping proof worker lost its claim".to_string(),
                    ));
                }
                next_heartbeat = std::time::Instant::now()
                    + std::time::Duration::from_secs(MAPPING_PROOF_HEARTBEAT_SECS);
            }
            let chunk_hash = Hash::from_hex(&chunk.hash)
                .map_err(|error| OakError::InvalidHash(error.to_string()))?;
            match repo.get_chunk(&chunk_hash)? {
                Some(bytes)
                    if bytes.len() == chunk.size as usize && hash_bytes(&bytes) == chunk_hash =>
                {
                    hasher.update(&bytes);
                }
                _ => content_present = false,
            }
            Ok(())
        })?;
        let blob_hash = Hash::from_hex(&descriptor.hash)
            .map_err(|error| ServeError::bad_request(error.to_string()))?;
        let current_base = if published.blobs.contains(&descriptor.hash) {
            Some(canonical_blob_mapping_digest(
                &repo.get_blob_chunks(&blob_hash)?.unwrap_or_default(),
            ))
        } else {
            None
        };
        if current_base != job.base_mapping_digests[index]
            && current_base.as_deref() != Some(descriptor.mapping_digest.as_str())
        {
            persist_mapping_generation_conflict(&repo, &token, &worker_token)?;
            return Ok(());
        }
        if let Some(existing) = repo.get_blob_chunks(&blob_hash)? {
            if canonical_blob_mapping_digest(&existing) != descriptor.mapping_digest {
                persist_mapping_generation_conflict(&repo, &token, &worker_token)?;
                return Ok(());
            }
        }
        if !content_present || hasher.finalize().to_hex().as_str() != blob_hash.as_str() {
            missing.push(descriptor.hash.clone());
        }
    }
    let verified: Vec<String> = job
        .descriptors
        .iter()
        .filter(|descriptor| !missing.contains(&descriptor.hash))
        .map(|descriptor| descriptor.hash.clone())
        .collect();
    let completed_at = chrono::Utc::now().timestamp();
    repo.write_txn_begin()?;
    let stored = (|| -> Result<()> {
        if missing.is_empty() {
            repo.activate_claimed_serve_mapping_proof_mappings(&token, &worker_token)?;
        }
        repo.complete_claimed_serve_mapping_proof(
            &token,
            &worker_token,
            &verified,
            &missing,
            completed_at,
        )
    })();
    if let Err(error) = stored {
        repo.write_txn_rollback();
        if is_mapping_generation_conflict(&error) {
            persist_mapping_generation_conflict(&repo, &token, &worker_token)?;
            return Ok(());
        }
        return Err(error.into());
    }
    if let Err(error) = repo.write_txn_commit() {
        repo.write_txn_rollback();
        return Err(error.into());
    }
    Ok(())
}

async fn finalize_blob_mapping_proof(
    State(state): State<ServeState>,
    AxPath((owner, name, token)): AxPath<(String, String, String)>,
    Json(_req): Json<BlobProofFinalizeRequest>,
) -> std::result::Result<(StatusCode, Json<BlobProofResponse>), ServeError> {
    let root = state.root.clone();
    let worker_root = root.clone();
    let worker_owner = owner.clone();
    let worker_name = name.clone();
    let worker_job_token = token.clone();
    let (mut job, worker_token) = blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let mut job = repo
            .load_serve_mapping_proof_header(&token)?
            .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
        mapping_proof_http_status(&repo, &job)?;
        if job.status == "complete" {
            return Ok((job, None));
        }
        let now = chrono::Utc::now().timestamp();
        if matches!(job.status.as_str(), "pending" | "running")
            && job.lease_expires_at.is_some_and(|lease| lease >= now)
        {
            return Ok((job, None));
        }
        repo.validate_serve_mapping_proof_pages(&job.token)
            .map_err(|error| ServeError::new(StatusCode::CONFLICT, error.to_string()))?;
        let worker_token = uuid::Uuid::new_v4().simple().to_string();
        let claimed = repo.claim_serve_mapping_proof(
            &job.token,
            &worker_token,
            now,
            now + MAPPING_PROOF_LEASE_SECS,
        )?;
        if claimed {
            job.status = "running".to_string();
            job.worker_token = Some(worker_token.clone());
            job.lease_expires_at = Some(now + MAPPING_PROOF_LEASE_SECS);
            Ok((job, Some(worker_token)))
        } else {
            let current = repo
                .load_serve_mapping_proof_header(&job.token)?
                .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
            mapping_proof_http_status(&repo, &current)?;
            Ok((current, None))
        }
    })
    .await?;
    if let Some(worker_token) = worker_token {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = run_blob_mapping_proof_worker(
                worker_root,
                worker_owner,
                worker_name,
                worker_job_token,
                worker_token,
            ) {
                eprintln!("oak serve mapping proof worker failed: {}", error.message);
            }
        });
    }
    let status = if job.status == "complete" {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    // Never expose worker ownership as part of the wire response.
    job.worker_token = None;
    job.lease_expires_at = None;
    Ok((status, Json(mapping_proof_response(&job))))
}

async fn poll_blob_mapping_proof(
    State(state): State<ServeState>,
    AxPath((owner, name, token)): AxPath<(String, String, String)>,
) -> std::result::Result<(StatusCode, Json<BlobProofResponse>), ServeError> {
    let root = state.root.clone();
    let worker_root = root.clone();
    let worker_owner = owner.clone();
    let worker_name = name.clone();
    let worker_job_token = token.clone();
    let (mut job, worker_token) = blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let mut job = repo
            .load_serve_mapping_proof_header(&token)?
            .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
        mapping_proof_http_status(&repo, &job)?;
        let now = chrono::Utc::now().timestamp();
        if matches!(job.status.as_str(), "pending" | "running")
            && job.lease_expires_at.is_none_or(|lease| lease < now)
        {
            let worker_token = uuid::Uuid::new_v4().simple().to_string();
            if repo.claim_serve_mapping_proof(
                &token,
                &worker_token,
                now,
                now + MAPPING_PROOF_LEASE_SECS,
            )? {
                job.status = "running".to_string();
                job.worker_token = Some(worker_token.clone());
                job.lease_expires_at = Some(now + MAPPING_PROOF_LEASE_SECS);
                return Ok((job, Some(worker_token)));
            }
            job = repo
                .load_serve_mapping_proof_header(&token)?
                .ok_or_else(|| ServeError::new(StatusCode::NOT_FOUND, "mapping proof not found"))?;
            mapping_proof_http_status(&repo, &job)?;
        }
        Ok((job, None))
    })
    .await?;
    if let Some(worker_token) = worker_token {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = run_blob_mapping_proof_worker(
                worker_root,
                worker_owner,
                worker_name,
                worker_job_token,
                worker_token,
            ) {
                eprintln!("oak serve mapping proof worker failed: {}", error.message);
            }
        });
    }
    let status = if job.status == "complete" {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    job.worker_token = None;
    job.lease_expires_at = None;
    Ok((status, Json(mapping_proof_response(&job))))
}

/// `POST /api/{owner}/{name}/chunks/check` — which of `hashes` the server
/// lacks. `upload_url` is always `None` (no R2), routing the client onto the
/// server-mediated `PUT /chunks/{hash}` path.
async fn check_chunks(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<ChunkCheckRequest>,
) -> std::result::Result<Json<ChunkCheckServerResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let published = published_object_closure(&repo)?;
        let bounded = match req.chunk_batch_protocol.as_deref() {
            None => false,
            Some(oak_core::protocol::CHUNK_BATCH_PROTOCOL) => true,
            Some(_) => return Err(ServeError::bad_request("unsupported chunk batch protocol")),
        };
        if (bounded && req.hashes.len() > oak_core::protocol::CHUNK_BATCH_MAX_HASHES)
            || req
                .sizes
                .as_ref()
                .is_some_and(|sizes| sizes.len() != req.hashes.len())
        {
            return Err(ServeError::bad_request(
                "chunk check exceeds admission cap or has mismatched sizes",
            ));
        }
        let mut unique = HashSet::with_capacity(req.hashes.len());
        let mut missing = Vec::new();
        for h in &req.hashes {
            let hash =
                Hash::from_hex(h).map_err(|error| ServeError::bad_request(error.to_string()))?;
            if bounded && !unique.insert(hash.clone()) {
                return Err(ServeError::bad_request(
                    "chunk check contains duplicate hash",
                ));
            }
            if !published.chunks.contains(hash.as_str()) || !repo.has_chunk(&hash)? {
                missing.push(ChunkUploadInfo {
                    hash: h.clone(),
                    upload_url: None,
                });
            }
        }
        Ok(Json(ChunkCheckServerResponse { missing }))
    })
    .await
}

/// `PUT /api/{owner}/{name}/chunks/{hash}` — store one chunk's raw bytes.
async fn upload_chunk(
    State(state): State<ServeState>,
    AxPath((owner, name, hash)): AxPath<(String, String, String)>,
    body: Bytes,
) -> std::result::Result<StatusCode, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        if body.len() > 64 * 1024 * 1024 {
            return Err(ServeError::bad_request("chunk exceeds 64 MiB upload cap"));
        }
        let hash = Hash::from_hex(&hash).map_err(|e| ServeError::bad_request(e.to_string()))?;
        // Bytes are untrusted: verify they hash to the claimed key before
        // they enter the content-addressed store. `chunk_decode` tolerates a
        // zstd-encoded body (decompresses only when the plaintext matches),
        // so both wire forms verify; plaintext is what gets stored.
        let content = oak_core::chunk_decode(body.to_vec(), &hash);
        if oak_core::hash_bytes(&content) != hash {
            return Err(ServeError::bad_request(format!(
                "chunk {hash} does not hash to its claimed key"
            )));
        }
        repo.store_chunk(&hash, &content)?;
        Ok(StatusCode::OK)
    })
    .await
}

async fn upload_chunk_batch(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    body: Bytes,
) -> std::result::Result<StatusCode, ServeError> {
    if body.len() > 64 * 1024 * 1024 {
        return Err(ServeError::bad_request("chunk batch exceeds 64 MiB cap"));
    }
    let mut cursor = body.as_ref();
    let mut entries = Vec::new();
    while !cursor.is_empty() {
        if entries.len() == 256 || cursor.len() < 4 {
            return Err(ServeError::bad_request(
                "malformed or oversized chunk batch",
            ));
        }
        let hash_len = u32::from_be_bytes(cursor[..4].try_into().unwrap()) as usize;
        cursor = &cursor[4..];
        if cursor.len() < hash_len + 4 {
            return Err(ServeError::bad_request("truncated chunk batch hash"));
        }
        let hash = std::str::from_utf8(&cursor[..hash_len])
            .map_err(|_| ServeError::bad_request("chunk batch hash is not UTF-8"))?;
        let hash =
            Hash::from_hex(hash).map_err(|error| ServeError::bad_request(error.to_string()))?;
        cursor = &cursor[hash_len..];
        let data_len = u32::from_be_bytes(cursor[..4].try_into().unwrap()) as usize;
        cursor = &cursor[4..];
        if cursor.len() < data_len {
            return Err(ServeError::bad_request("truncated chunk batch content"));
        }
        let data = cursor[..data_len].to_vec();
        cursor = &cursor[data_len..];
        if hash_bytes(&data) != hash {
            return Err(ServeError::bad_request(format!(
                "chunk {hash} content hash mismatch"
            )));
        }
        entries.push((hash, data));
    }
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let bulk = super::BulkTxn::begin(&repo)?;
        for (hash, data) in entries {
            repo.store_chunk(&hash, &data)?;
        }
        bulk.commit()?;
        Ok(StatusCode::OK)
    })
    .await
}

/// `POST /api/{owner}/{name}/chunks/download` — inline chunk content (no R2, so
/// never a presigned URL). Missing hashes are silently omitted.
async fn download_chunks(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<ChunkDownloadRequest>,
) -> std::result::Result<Json<ChunkDownloadResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let published = published_object_closure(&repo)?;
        let bounded = match req.chunk_batch_protocol.as_deref() {
            None => false,
            Some(oak_core::protocol::CHUNK_BATCH_PROTOCOL) => true,
            Some(_) => return Err(ServeError::bad_request("unsupported chunk batch protocol")),
        };
        if bounded && req.hashes.len() > oak_core::protocol::CHUNK_BATCH_MAX_HASHES {
            return Err(ServeError::bad_request(
                "chunk download exceeds 10000-hash cap",
            ));
        }
        let mut unique = HashSet::with_capacity(req.hashes.len());
        let mut chunks = Vec::new();
        for h in &req.hashes {
            let hash =
                Hash::from_hex(h).map_err(|error| ServeError::bad_request(error.to_string()))?;
            if bounded && !unique.insert(hash.clone()) {
                return Err(ServeError::bad_request(
                    "chunk download contains duplicate hash",
                ));
            }
            if published.chunks.contains(hash.as_str()) {
                let Some(content) = repo.get_chunk(&hash)? else {
                    continue;
                };
                chunks.push(ChunkDownloadInfo {
                    hash: h.clone(),
                    download_url: None,
                    content: Some(content),
                });
            }
        }
        // `oak serve` has no chunk CDN — clients use the inline content above.
        Ok(Json(ChunkDownloadResponse {
            chunks,
            batch_url: None,
            restricted: Vec::new(),
        }))
    })
    .await
}

/// `POST /api/{owner}/{name}/chunks/uploaded` — a no-op here. On the hosted
/// server this confirms a presigned-R2 upload landed; with the server-mediated
/// path the bytes are already durable from `PUT /chunks/{hash}`.
async fn confirm_chunks_uploaded(
    State(_state): State<ServeState>,
    AxPath((_owner, _name)): AxPath<(String, String)>,
    Json(req): Json<ChunkUploadedRequest>,
) -> std::result::Result<StatusCode, ServeError> {
    if req.hashes.len() > 256
        || req.hashes.iter().any(|entry| entry.size > 64 * 1024 * 1024)
        || req
            .hashes
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.size))
            > 256 * 1024 * 1024
    {
        return Err(ServeError::bad_request(
            "chunk confirmation exceeds admission cap",
        ));
    }
    let mut unique = HashSet::with_capacity(req.hashes.len());
    for entry in req.hashes {
        let hash = Hash::from_hex(&entry.hash)
            .map_err(|error| ServeError::bad_request(error.to_string()))?;
        if !unique.insert(hash) {
            return Err(ServeError::bad_request(
                "chunk confirmation contains duplicate hash",
            ));
        }
    }
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Auth middleware + router
// ---------------------------------------------------------------------------

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max_len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for idx in 0..max_len {
        let left = a.get(idx).copied().unwrap_or(0);
        let right = b.get(idx).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

/// Optional shared-secret gate. Active only when `--token` is set.
async fn require_token(
    State(state): State<ServeState>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, ServeError> {
    if let Some(expected) = &state.token {
        let presented = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        if presented.is_none_or(|presented| !constant_time_eq(presented, expected)) {
            return Err(ServeError::new(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            ));
        }
    }
    Ok(next.run(request).await)
}

fn build_router(state: ServeState) -> Router {
    Router::new()
        .route("/api/capabilities", get(capabilities))
        .route(
            "/api/repos",
            post(create_repo).layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route("/api/{owner}/{name}", get(get_repo))
        .route("/api/{owner}/{name}/push", post(push))
        .route("/api/{owner}/{name}/push/staged-v1", post(staged_push_v1))
        .route(
            "/api/{owner}/{name}/push/staged-v1/{stage_id}/abort",
            post(abort_staged_push_v1).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/api/{owner}/{name}/pull", get(pull))
        .route(
            "/api/{owner}/{name}/commits/info",
            post(commit_info).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/{owner}/{name}/branches/{branch}", get(get_branch))
        .route(
            "/api/{owner}/{name}/blobs/check",
            post(check_blobs).layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/blobs/proofs/async-v1",
            post(create_blob_mapping_proof_async_v1)
                .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/blobs/proofs/{token}/mappings",
            put(upload_blob_mapping_pages)
                .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/blobs/proofs/{token}/finalize",
            post(finalize_blob_mapping_proof).layer(axum::extract::DefaultBodyLimit::max(
                oak_core::protocol::MAPPING_PROOF_FINALIZE_BODY_BYTES,
            )),
        )
        .route(
            "/api/{owner}/{name}/blobs/proofs/{token}",
            get(poll_blob_mapping_proof),
        )
        .route(
            "/api/{owner}/{name}/chunks/check",
            post(check_chunks).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/chunks/{hash}",
            put(upload_chunk).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/chunks/batch",
            post(upload_chunk_batch).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/chunks/download",
            post(download_chunks).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/{owner}/{name}/chunks/uploaded",
            post(confirm_chunks_uploaded)
                .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        // Allow large blob/chunk uploads (default axum limit is 2 MB).
        .layer(axum::extract::DefaultBodyLimit::max(512 * 1024 * 1024))
        .with_state(state)
}

async fn resume_mapping_proof_workers(root: PathBuf) -> Result<()> {
    let scan_root = root.clone();
    let claims = blocking(move || {
        let mut claims = Vec::new();
        let owners = std::fs::read_dir(&scan_root)
            .map_err(|error| ServeError::internal(format!("scan Serve data root: {error}")))?;
        for owner_entry in owners.flatten().filter(|entry| entry.path().is_dir()) {
            let owner = owner_entry.file_name().to_string_lossy().to_string();
            if check_segment(&owner).is_err() {
                continue;
            }
            let repos = match std::fs::read_dir(owner_entry.path()) {
                Ok(repos) => repos,
                Err(_) => continue,
            };
            for repo_entry in repos.flatten().filter(|entry| entry.path().is_file()) {
                let filename = repo_entry.file_name().to_string_lossy().to_string();
                let Some(name) = filename.strip_suffix(".oakdb").map(str::to_string) else {
                    continue;
                };
                if check_segment(&name).is_err() {
                    continue;
                }
                let repo = open_existing(&scan_root, &owner, &name)?;
                let now = chrono::Utc::now().timestamp();
                for token in repo.list_resumable_serve_mapping_proofs(now)? {
                    let worker_token = uuid::Uuid::new_v4().simple().to_string();
                    if repo.claim_serve_mapping_proof(
                        &token,
                        &worker_token,
                        now,
                        now + MAPPING_PROOF_LEASE_SECS,
                    )? {
                        claims.push((owner.clone(), name.clone(), token, worker_token));
                    }
                }
            }
        }
        Ok(claims)
    })
    .await
    .map_err(|error| OakError::Server(error.message))?;
    for (owner, name, token, worker_token) in claims {
        let worker_root = root.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) =
                run_blob_mapping_proof_worker(worker_root, owner, name, token, worker_token)
            {
                eprintln!(
                    "oak serve resumed mapping proof worker failed: {}",
                    error.message
                );
            }
        });
    }
    Ok(())
}

/// Bind the reference server on an ephemeral loopback port, drive it on a
/// background task, and return its base URL (`http://127.0.0.1:<port>`).
///
/// [`run`] prints its bound address rather than returning it, which makes it
/// unusable from a test that needs to point a client at the server it just
/// started. This is the same router and the same handlers — only the plumbing
/// differs — so end-to-end tests exercise the real push/pull wire path.
pub async fn spawn_loopback(dir: PathBuf) -> Result<String> {
    std::fs::create_dir_all(&dir)?;
    let root = dir.canonicalize().unwrap_or(dir);
    resume_mapping_proof_workers(root.clone()).await?;
    let app = build_router(ServeState { root, token: None });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OakError::Server(format!("failed to bind loopback: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| OakError::Server(e.to_string()))?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}"))
}

/// Entry point for `oak serve` (run on the CLI's shared tokio runtime).
pub async fn run(dir: PathBuf, host: IpAddr, port: u16, token: Option<String>) -> Result<()> {
    let token = match token {
        Some(token) if token.trim().is_empty() => {
            return Err(OakError::Server(
                "--token must not be empty or whitespace".to_string(),
            ));
        }
        other => other,
    };
    if !host.is_loopback() && token.is_none() {
        return Err(OakError::Server(
            "refusing to bind oak serve on a non-loopback address without --token".to_string(),
        ));
    }
    std::fs::create_dir_all(&dir)?;
    let root = dir.canonicalize().unwrap_or(dir);
    let has_token = token.is_some();
    let state = ServeState {
        root: root.clone(),
        token: token.map(Arc::new),
    };
    resume_mapping_proof_workers(root.clone()).await?;
    let app = build_router(state);

    let addr = SocketAddr::new(host, port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| OakError::Server(format!("failed to bind {addr}: {e}")))?;

    crate::output::print_line(&format!("oak serve — data dir: {}", root.display()));
    crate::output::print_line(&format!("listening on http://{addr}  (Ctrl-C to stop)"));
    crate::output::print_line(&format!(
        "auth: {}",
        if has_token {
            "shared bearer token required"
        } else {
            "open on loopback only (use --token before widening --host)"
        }
    ));
    crate::output::print_line("warning: owner paths are namespaces only; any accepted client can access every repo under this data dir");
    crate::output::print_line(&format!(
        "clone with:  oak clone http://<host>:{port}/<owner>/<name>"
    ));

    axum::serve(listener, app)
        .await
        .map_err(|e| OakError::Server(format!("server error: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use oak_core::protocol::{BlobProofMappingPage, BranchPushData};
    use oak_core::{Branch, Commit};

    /// A reqwest client for tests. reqwest is on `rustls-no-provider`, so
    /// building a client panics unless the process-default crypto provider is
    /// installed first (these tests don't go through `main`).
    fn test_client() -> reqwest::Client {
        crate::http::ensure_crypto_provider();
        reqwest::Client::new()
    }

    #[test]
    fn token_compare_matches_exact_values_only() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreu"));
        assert!(!constant_time_eq("secret", "secret-longer"));
        assert!(!constant_time_eq("secret-longer", "secret"));
    }

    #[test]
    fn staged_session_cumulative_caps_cover_every_operation_dimension() {
        let cases = [
            ServeStageTotals {
                commits: oak_core::protocol::STAGED_OPERATION_MAX_COMMITS + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                trees: oak_core::protocol::STAGED_MAX_TREE_OBJECTS + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                direct_tree_entries: oak_core::protocol::STAGED_MAX_DIRECT_TREE_ENTRIES + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                resolved_manifest_entries: oak_core::protocol::STAGED_MAX_RESOLVED_MANIFEST_ENTRIES
                    + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                file_changes: oak_core::protocol::STAGED_MAX_FILE_CHANGES + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                canonical_metadata_bytes: oak_core::protocol::STAGED_MAX_CANONICAL_METADATA_BYTES
                    + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                expanded_path_bytes: oak_core::protocol::STAGED_MAX_EXPANDED_PATH_BYTES + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                chunk_refs: oak_core::protocol::STAGED_MAX_CHUNK_REFS + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                blobs: oak_core::protocol::STAGED_MAX_BLOBS + 1,
                ..ServeStageTotals::default()
            },
            ServeStageTotals {
                declared_blob_bytes: oak_core::protocol::STAGED_MAX_DECLARED_BLOB_BYTES + 1,
                ..ServeStageTotals::default()
            },
        ];
        for totals in cases {
            assert!(validate_stage_operation_totals(&totals).is_err());
        }
    }

    #[test]
    fn completed_session_tombstones_outlive_active_receipts_then_expire() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let now = Utc::now().timestamp();
        let stage_id = "completed-session-0123456789abcdef".to_string();
        let mut sessions = HashMap::from([(
            stage_id.clone(),
            ServeStageSession {
                branch: "main".to_string(),
                updated_at: now - STAGED_SESSION_TTL_SECS - 1,
                state: ServeStageState::Completed,
                completed_target: Some("ab".repeat(32)),
                ..ServeStageSession::default()
            },
        )]);
        store_stage_sessions(&repo, &sessions).unwrap();
        assert!(load_active_stage_sessions(&repo)
            .unwrap()
            .contains_key(&stage_id));

        sessions.get_mut(&stage_id).unwrap().updated_at =
            now - oak_core::protocol::STAGED_COMPLETED_TOMBSTONE_TTL_SECS - 1;
        store_stage_sessions(&repo, &sessions).unwrap();
        assert!(!load_active_stage_sessions(&repo)
            .unwrap()
            .contains_key(&stage_id));
    }

    #[test]
    fn completed_tombstone_retention_is_count_and_record_size_bounded() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let now = Utc::now().timestamp();
        let large_membership: HashSet<String> =
            (0..10_000).map(|index| format!("{index:064x}")).collect();
        let mut large = ServeStageSession {
            branch: "main".to_string(),
            branch_identity: "ab".repeat(32),
            expected_head: Some("cd".repeat(32)),
            totals: ServeStageTotals {
                commits: 10_000,
                trees: 10_000,
                blobs: 10_000,
                chunk_refs: 10_000,
                ..ServeStageTotals::default()
            },
            manifest_roots: large_membership.clone(),
            commits: large_membership.clone(),
            trees: large_membership.clone(),
            blobs: large_membership.clone(),
            chunks: large_membership,
            ..ServeStageSession::default()
        };
        assert!(serde_json::to_vec(&large).unwrap().len() > 1024 * 1024);
        compact_completed_stage_session(&mut large, "ef".repeat(32), now);
        assert!(serde_json::to_vec(&large).unwrap().len() < 512);

        let mut sessions = HashMap::new();
        for index in 0..(MAX_COMPLETED_STAGE_TOMBSTONES + 50) {
            sessions.insert(
                format!("completed-{index:04}-0123456789abcdef"),
                ServeStageSession {
                    branch: "main".to_string(),
                    branch_identity: "ab".repeat(32),
                    expected_head: Some("cd".repeat(32)),
                    updated_at: now - index as i64,
                    state: ServeStageState::Completed,
                    completed_target: Some("ef".repeat(32)),
                    ..ServeStageSession::default()
                },
            );
        }
        store_stage_sessions(&repo, &sessions).unwrap();
        let retained = load_active_stage_sessions(&repo).unwrap();
        assert_eq!(retained.len(), MAX_COMPLETED_STAGE_TOMBSTONES);
        assert!(retained.values().all(|session| {
            serde_json::to_vec(session).unwrap().len() < 512
                && session.commits.is_empty()
                && session.trees.is_empty()
                && session.blobs.is_empty()
                && session.chunks.is_empty()
                && session.manifest_roots.is_empty()
                && session.totals == ServeStageTotals::default()
        }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_refuses_non_loopback_without_token() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path().join("data"), "0.0.0.0".parse().unwrap(), 0, None)
            .await
            .expect_err("non-loopback bind without token must fail before listening");

        assert!(
            err.to_string().contains("non-loopback"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_rejects_empty_token_before_listening() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            dir.path().join("data"),
            "0.0.0.0".parse().unwrap(),
            0,
            Some("  ".to_string()),
        )
        .await
        .expect_err("empty token must not count as public-bind protection");

        assert!(
            err.to_string().contains("must not be empty"),
            "unexpected error: {err}"
        );
    }

    /// Spin the real router on an ephemeral port; returns its base URL.
    async fn spawn_server(root: PathBuf) -> String {
        resume_mapping_proof_workers(root.clone()).await.unwrap();
        let state = ServeState { root, token: None };
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Seed `oak/oak` with one branch holding one commit, mirroring a repo
    /// that has already received a real push. Returns the branch head.
    fn seed_branch(root: &Path, branch: &str, desc: &str) -> Hash {
        let repo = open_for_write(root, "oak", "oak").unwrap();
        repo.store_branch(&Branch::new(
            branch.to_string(),
            Some(desc.to_string()),
            Some("main".to_string()),
        ))
        .unwrap();
        let manifest_hash = repo.put_manifest(Vec::new()).unwrap();
        let commit = Commit::new(
            branch.to_string(),
            None,
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Vec::new(),
        )
        .unwrap();
        let head = commit.hash.clone();
        repo.store_commit(&commit).unwrap();
        repo.set_branch_head(branch, &head).unwrap();
        repo.set_head(&head).unwrap();
        head
    }

    fn metadata_only_push(branch: &str, desc: &str) -> PushRequest {
        metadata_only_push_with_status(branch, desc, "open", None)
    }

    fn metadata_only_push_with_status(
        branch: &str,
        desc: &str,
        status: &str,
        close_reason: Option<&str>,
    ) -> PushRequest {
        PushRequest {
            expected_head: None,
            expected_branch_head: None,
            force: false,
            branch: Some(BranchPushData {
                name: branch.to_string(),
                description: Some(desc.to_string()),
                parent_branch: Some("main".to_string()),
                status: status.to_string(),
                created_at: Utc::now().to_rfc3339(),
                close_reason: close_reason.map(str::to_string),
            }),
            commits: Vec::new(),
            blobs: Vec::new(),
            trees: Vec::new(),
        }
    }

    fn staged_request(
        commits: Vec<CommitData>,
        trees: Vec<oak_core::protocol::TreeData>,
        blobs: Vec<BlobData>,
    ) -> StagedPushRequest {
        StagedPushRequest {
            stage_id: "0123456789abcdef0123456789abcdef".to_string(),
            expected_branch_head: None,
            branch: BranchPushData {
                name: "main".to_string(),
                description: Some("main".to_string()),
                parent_branch: None,
                status: "open".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: None,
            },
            finalize: false,
            force: false,
            target_head: None,
            commits,
            blobs,
            trees,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_force_rewrite_preserves_the_expected_head_cas() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        let boundary = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            Some("published boundary".to_string()),
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        repo.store_commit(&boundary).unwrap();
        repo.set_branch_head("main", &boundary.hash).unwrap();
        let rewritten = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            Some("deliberate rewrite".to_string()),
            Vec::new(),
            Utc::now() + chrono::Duration::seconds(1),
        )
        .unwrap();

        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let mut stage = staged_request(vec![commit_to_wire(&rewritten)], Vec::new(), Vec::new());
        stage.expected_branch_head = Some(boundary.hash.to_string());
        stage.force = true;
        let response = test_client()
            .post(&endpoint)
            .json(&stage)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut finalize = staged_request(Vec::new(), Vec::new(), Vec::new());
        finalize.expected_branch_head = Some(boundary.hash.to_string());
        finalize.force = true;
        finalize.finalize = true;
        finalize.target_head = Some(rewritten.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&finalize)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            repo.get_branch_head("main").unwrap(),
            Some(rewritten.hash.clone())
        );

        let rejected = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            Some("unforced rewrite".to_string()),
            Vec::new(),
            Utc::now() + chrono::Duration::seconds(2),
        )
        .unwrap();
        let mut unforced = staged_request(vec![commit_to_wire(&rejected)], Vec::new(), Vec::new());
        unforced.stage_id = "fedcba9876543210fedcba9876543210".to_string();
        unforced.expected_branch_head = Some(rewritten.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&unforced)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut unforced_finalize = staged_request(Vec::new(), Vec::new(), Vec::new());
        unforced_finalize.stage_id = unforced.stage_id.clone();
        unforced_finalize.expected_branch_head = Some(rewritten.hash.to_string());
        unforced_finalize.finalize = true;
        unforced_finalize.target_head = Some(rejected.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&unforced_finalize)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            repo.get_branch_head("main").unwrap(),
            Some(rewritten.hash.clone())
        );

        unforced_finalize.force = true;
        let response = test_client()
            .post(&endpoint)
            .json(&unforced_finalize)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(repo.get_branch_head("main").unwrap(), Some(rewritten.hash));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_staged_request_does_not_create_a_repository() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let base = spawn_server(root.clone()).await;
        let missing_parent = Hash("ab".repeat(32));
        let orphan = Commit::with_timestamp(
            "main".to_string(),
            Some(missing_parent),
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        let response = test_client()
            .post(format!("{base}/api/oak/fresh/push/staged-v1"))
            .json(&staged_request(
                vec![commit_to_wire(&orphan)],
                vec![],
                vec![],
            ))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(!root.join("oak/fresh.oakdb").exists());

        let response = test_client()
            .get(format!("{base}/api/oak/fresh"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!root.join("oak/fresh.oakdb").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_v1_proof_reserves_exact_mapping_without_publishing_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let left = b"abc".to_vec();
        let right = b"def".to_vec();
        let whole = b"abcdef".to_vec();
        let left_hash = hash_bytes(&left);
        let right_hash = hash_bytes(&right);
        let whole_hash = hash_bytes(&whole);
        repo.store_chunk(&left_hash, &left).unwrap();
        repo.store_chunk(&right_hash, &right).unwrap();
        repo.store_chunk(&whole_hash, &whole).unwrap();
        let base = spawn_server(root).await;

        let proof_chunks = vec![
            oak_core::protocol::BlobProofChunk {
                hash: left_hash.to_string(),
                offset: 0,
                size: 3,
            },
            oak_core::protocol::BlobProofChunk {
                hash: right_hash.to_string(),
                offset: 3,
                size: 3,
            },
        ];
        let proof_descriptor = oak_core::protocol::BlobProofDescriptor {
            hash: whole_hash.to_string(),
            size: 6,
            mapping_digest: oak_core::protocol::blob_mapping_digest(&proof_chunks),
            total_chunks: 2,
        };

        let response = test_client()
            .post(format!("{base}/api/oak/oak/blobs/proofs/async-v1"))
            .json(&BlobProofRequest {
                blobs: vec![proof_descriptor.clone()],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let created: BlobProofResponse = response.json().await.unwrap();
        let proof_token = created.mapping_proof_job.unwrap().token;

        let duplicate_create = test_client()
            .post(format!("{base}/api/oak/oak/blobs/proofs/async-v1"))
            .json(&BlobProofRequest {
                blobs: vec![proof_descriptor],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate_create.status(), StatusCode::ACCEPTED);
        let duplicate: BlobProofResponse = duplicate_create.json().await.unwrap();
        assert_eq!(duplicate.mapping_proof_job.unwrap().token, proof_token);

        let mapping_url = format!("{base}/api/oak/oak/blobs/proofs/{proof_token}/mappings");
        let pages = BlobProofPagesRequest {
            pages: vec![oak_core::protocol::BlobProofMappingPage {
                blob_index: 0,
                first_chunk_index: 0,
                chunks: proof_chunks,
            }],
        };
        let uploaded = test_client()
            .put(&mapping_url)
            .json(&BlobProofPagesRequest {
                pages: pages.pages.clone(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(uploaded.status(), StatusCode::OK);
        let replayed = test_client()
            .put(&mapping_url)
            .json(&pages)
            .send()
            .await
            .unwrap();
        assert_eq!(replayed.status(), StatusCode::OK);
        let conflicting = test_client()
            .put(&mapping_url)
            .json(&BlobProofPagesRequest {
                pages: vec![oak_core::protocol::BlobProofMappingPage {
                    blob_index: 0,
                    first_chunk_index: 0,
                    chunks: vec![oak_core::protocol::BlobProofChunk {
                        hash: whole_hash.to_string(),
                        offset: 0,
                        size: 3,
                    }],
                }],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(conflicting.status(), StatusCode::CONFLICT);

        let finalize_url = format!("{base}/api/oak/oak/blobs/proofs/{proof_token}/finalize");
        let response = test_client()
            .post(&finalize_url)
            .json(&BlobProofFinalizeRequest {})
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let pending: BlobProofResponse = response.json().await.unwrap();
        assert_eq!(pending.mapping_proof_job.unwrap().token, proof_token);
        let duplicate_finalize = test_client()
            .post(&finalize_url)
            .json(&BlobProofFinalizeRequest {})
            .send()
            .await
            .unwrap();
        assert!(matches!(
            duplicate_finalize.status(),
            StatusCode::ACCEPTED | StatusCode::OK
        ));
        let mut proof = None;
        for _ in 0..100 {
            let response = test_client()
                .get(format!("{base}/api/oak/oak/blobs/proofs/{proof_token}"))
                .send()
                .await
                .unwrap();
            if response.status() == StatusCode::OK {
                proof = Some(response.json::<BlobProofResponse>().await.unwrap());
                break;
            }
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let proof = proof.expect("background mapping proof should complete");
        assert_eq!(proof.verified, vec![whole_hash.to_string()]);
        assert_eq!(proof.proof_token.as_deref(), Some(proof_token.as_str()));
        let replayed_response = test_client()
            .post(&finalize_url)
            .json(&BlobProofFinalizeRequest {})
            .send()
            .await
            .unwrap();
        assert_eq!(replayed_response.status(), StatusCode::OK);
        let replayed: BlobProofResponse = replayed_response.json().await.unwrap();
        assert_eq!(replayed.proof_token.as_deref(), Some(proof_token.as_str()));
        let terminal_page_replay = test_client()
            .put(&mapping_url)
            .json(&pages)
            .send()
            .await
            .unwrap();
        assert_eq!(terminal_page_replay.status(), StatusCode::OK);
        let terminal_page_mismatch = test_client()
            .put(&mapping_url)
            .json(&BlobProofPagesRequest {
                pages: vec![oak_core::protocol::BlobProofMappingPage {
                    blob_index: 0,
                    first_chunk_index: 0,
                    chunks: vec![oak_core::protocol::BlobProofChunk {
                        hash: whole_hash.to_string(),
                        offset: 0,
                        size: 3,
                    }],
                }],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(terminal_page_mismatch.status(), StatusCode::CONFLICT);
        let polled: BlobProofResponse = test_client()
            .get(format!("{base}/api/oak/oak/blobs/proofs/{proof_token}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(polled.proof_token.as_deref(), Some(proof_token.as_str()));

        let unpublished: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![whole_hash.to_string()],
                require_verified_receipts: false,
                verify_content: true,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unpublished.missing, vec![whole_hash.to_string()]);

        let chunk_refs = vec![
            ChunkRefData {
                hash: left_hash.to_string(),
                offset: 0,
                size: 3,
            },
            ChunkRefData {
                hash: right_hash.to_string(),
                offset: 3,
                size: 3,
            },
        ];
        let wrong_token = test_client()
            .post(format!("{base}/api/oak/oak/push/staged-v1"))
            .json(&staged_request(
                Vec::new(),
                Vec::new(),
                vec![BlobData {
                    hash: whole_hash.to_string(),
                    content: Vec::new(),
                    size: 6,
                    chunks: chunk_refs.clone(),
                    mapping_proof_token: Some("wrong".to_string()),
                }],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_token.status(), StatusCode::CONFLICT);

        let staged = test_client()
            .post(format!("{base}/api/oak/oak/push/staged-v1"))
            .json(&staged_request(
                Vec::new(),
                Vec::new(),
                vec![BlobData {
                    hash: whole_hash.to_string(),
                    content: Vec::new(),
                    size: 6,
                    chunks: chunk_refs,
                    mapping_proof_token: Some(proof_token),
                }],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(staged.status(), StatusCode::OK);

        let alternate = test_client()
            .post(format!("{base}/api/oak/oak/blobs/proofs/async-v1"))
            .json(&BlobProofRequest {
                blobs: vec![oak_core::protocol::BlobProofDescriptor {
                    hash: whole_hash.to_string(),
                    size: 6,
                    mapping_digest: oak_core::protocol::blob_mapping_digest(&[
                        oak_core::protocol::BlobProofChunk {
                            hash: whole_hash.to_string(),
                            offset: 0,
                            size: 6,
                        },
                    ]),
                    total_chunks: 1,
                }],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(alternate.status(), StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mapping_generation_race_is_a_fenced_terminal_conflict_and_fresh_request_can_retry() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let left = b"abc";
        let right = b"def";
        let whole = b"abcdef";
        let left_hash = hash_bytes(left);
        let right_hash = hash_bytes(right);
        let whole_hash = hash_bytes(whole);
        repo.store_chunk(&left_hash, left).unwrap();
        repo.store_chunk(&right_hash, right).unwrap();
        let desired_chunks = vec![
            BlobProofChunk {
                hash: left_hash.to_string(),
                offset: 0,
                size: 3,
            },
            BlobProofChunk {
                hash: right_hash.to_string(),
                offset: 3,
                size: 3,
            },
        ];
        let desired = BlobProofDescriptor {
            hash: whole_hash.to_string(),
            size: 6,
            mapping_digest: oak_core::protocol::blob_mapping_digest(&desired_chunks),
            total_chunks: 2,
        };
        let base = spawn_server(root).await;
        let create_url = format!("{base}/api/oak/oak/blobs/proofs/async-v1");
        let create_body = BlobProofRequest {
            blobs: vec![desired],
        };
        let created: BlobProofResponse = test_client()
            .post(&create_url)
            .json(&create_body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = created.mapping_proof_job.unwrap().token;
        test_client()
            .put(format!("{base}/api/oak/oak/blobs/proofs/{token}/mappings"))
            .json(&BlobProofPagesRequest {
                pages: vec![BlobProofMappingPage {
                    blob_index: 0,
                    first_chunk_index: 0,
                    chunks: desired_chunks,
                }],
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        // A concurrent writer installs a different valid segmentation after
        // this proof captured its base generation.
        repo.store_chunk(&whole_hash, whole).unwrap();
        repo.store_blob_chunks(
            &whole_hash,
            &[ChunkInfo {
                hash: whole_hash.clone(),
                offset: 0,
                length: 6,
            }],
        )
        .unwrap();

        let finalize_url = format!("{base}/api/oak/oak/blobs/proofs/{token}/finalize");
        assert_eq!(
            test_client()
                .post(&finalize_url)
                .json(&BlobProofFinalizeRequest {})
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        let poll_url = format!("{base}/api/oak/oak/blobs/proofs/{token}");
        let conflict = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let response = test_client().get(&poll_url).send().await.unwrap();
                if response.status() == StatusCode::CONFLICT {
                    break response;
                }
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("generation conflict must become terminal");
        assert_eq!(
            conflict.json::<ErrorResponse>().await.unwrap().error,
            oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT
        );
        for response in [
            test_client().get(&poll_url).send().await.unwrap(),
            test_client()
                .post(&finalize_url)
                .json(&BlobProofFinalizeRequest {})
                .send()
                .await
                .unwrap(),
            test_client()
                .post(&create_url)
                .json(&create_body)
                .send()
                .await
                .unwrap(),
        ] {
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.json::<ErrorResponse>().await.unwrap().error,
                oak_core::protocol::MAPPING_PROOF_GENERATION_CONFLICT
            );
        }
        assert!(!repo
            .list_resumable_serve_mapping_proofs(Utc::now().timestamp() + 10_000)
            .unwrap()
            .contains(&token));

        // The newly canonical mapping is a distinct request identity and can
        // retry while the old conflict tombstone remains deterministic.
        let fresh_chunks = vec![BlobProofChunk {
            hash: whole_hash.to_string(),
            offset: 0,
            size: 6,
        }];
        let fresh: BlobProofResponse = test_client()
            .post(&create_url)
            .json(&BlobProofRequest {
                blobs: vec![BlobProofDescriptor {
                    hash: whole_hash.to_string(),
                    size: 6,
                    mapping_digest: oak_core::protocol::blob_mapping_digest(&fresh_chunks),
                    total_chunks: 1,
                }],
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let fresh_token = fresh.mapping_proof_job.unwrap().token;
        assert_ne!(fresh_token, token);
        test_client()
            .put(format!(
                "{base}/api/oak/oak/blobs/proofs/{fresh_token}/mappings"
            ))
            .json(&BlobProofPagesRequest {
                pages: vec![BlobProofMappingPage {
                    blob_index: 0,
                    first_chunk_index: 0,
                    chunks: fresh_chunks,
                }],
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        test_client()
            .post(format!(
                "{base}/api/oak/oak/blobs/proofs/{fresh_token}/finalize"
            ))
            .json(&BlobProofFinalizeRequest {})
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let fresh_poll = format!("{base}/api/oak/oak/blobs/proofs/{fresh_token}");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let response = test_client().get(&fresh_poll).send().await.unwrap();
                if response.status() == StatusCode::OK {
                    let proof: BlobProofResponse = response.json().await.unwrap();
                    assert_eq!(proof.proof_token.as_deref(), Some(fresh_token.as_str()));
                    break;
                }
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fresh generation proof must complete deterministically");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_missing_proof_reverifies_same_request_after_content_repair() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let bytes = b"repaired proof content";
        let hash = hash_bytes(bytes);
        let chunk = BlobProofChunk {
            hash: hash.to_string(),
            offset: 0,
            size: bytes.len() as u32,
        };
        let descriptor = BlobProofDescriptor {
            hash: hash.to_string(),
            size: bytes.len() as u64,
            mapping_digest: oak_core::protocol::blob_mapping_digest(std::slice::from_ref(&chunk)),
            total_chunks: 1,
        };
        let create_url = format!("{base}/api/oak/oak/blobs/proofs/async-v1");
        let create_request = BlobProofRequest {
            blobs: vec![descriptor],
        };
        let created: BlobProofResponse = test_client()
            .post(&create_url)
            .json(&create_request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = created.mapping_proof_job.unwrap().token;
        let mapping_url = format!("{base}/api/oak/oak/blobs/proofs/{token}/mappings");
        let pages = BlobProofPagesRequest {
            pages: vec![oak_core::protocol::BlobProofMappingPage {
                blob_index: 0,
                first_chunk_index: 0,
                chunks: vec![chunk],
            }],
        };
        assert_eq!(
            test_client()
                .put(&mapping_url)
                .json(&pages)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let finalize_url = format!("{base}/api/oak/oak/blobs/proofs/{token}/finalize");
        assert_eq!(
            test_client()
                .post(&finalize_url)
                .json(&BlobProofFinalizeRequest {})
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        let poll_url = format!("{base}/api/oak/oak/blobs/proofs/{token}");
        let missing = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let response = test_client().get(&poll_url).send().await.unwrap();
                if response.status() == StatusCode::OK {
                    break response.json::<BlobProofResponse>().await.unwrap();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(missing.missing, vec![hash.to_string()]);

        let unchanged: BlobProofResponse = test_client()
            .post(&create_url)
            .json(&create_request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unchanged.missing, vec![hash.to_string()]);

        assert_eq!(
            test_client()
                .put(format!("{base}/api/oak/oak/chunks/{hash}"))
                .body(bytes.to_vec())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let restarted: BlobProofResponse = test_client()
            .post(&create_url)
            .json(&create_request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let restarted_job = restarted.mapping_proof_job.unwrap();
        assert_eq!(restarted_job.token, token);
        assert_eq!(restarted_job.status, "uploading");
        assert_eq!(
            test_client()
                .put(&mapping_url)
                .json(&pages)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            test_client()
                .post(&finalize_url)
                .json(&BlobProofFinalizeRequest {})
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        let verified = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let response = test_client().get(&poll_url).send().await.unwrap();
                if response.status() == StatusCode::OK {
                    break response.json::<BlobProofResponse>().await.unwrap();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(verified.verified, vec![hash.to_string()]);
        assert_eq!(verified.proof_token.as_deref(), Some(token.as_str()));
    }

    #[test]
    fn mapping_proof_jobs_preserve_expired_running_work_for_lease_reclamation() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("proofs.oakdb")).unwrap();
        let now = Utc::now().timestamp();
        let descriptor = BlobProofDescriptor {
            hash: "a".repeat(64),
            size: 3,
            mapping_digest: "b".repeat(64),
            total_chunks: 1,
        };
        let job = |token: &str, status: &str, updated_at: i64| ServeMappingProofJob {
            token: token.to_string(),
            request_digest: format!("request-{token}"),
            status: status.to_string(),
            worker_token: None,
            lease_expires_at: None,
            created_at: updated_at,
            updated_at,
            descriptors: vec![descriptor.clone()],
            base_mapping_digests: vec![None],
            mappings: vec![BTreeMap::new()],
            verified: Vec::new(),
            missing: Vec::new(),
        };
        let mut live_leased = job("running-live", "running", now - 2 * 60 * 60);
        live_leased.worker_token = Some("live-worker".to_string());
        live_leased.lease_expires_at = Some(now + 30);
        let jobs = HashMap::from([
            (
                "pending-live".to_string(),
                job("pending-live", "uploading", now - 60 * 60 + 1),
            ),
            (
                "pending-expired".to_string(),
                job("pending-expired", "running", now - 60 * 60 - 1),
            ),
            (
                "complete-live".to_string(),
                job("complete-live", "complete", now - 24 * 60 * 60 + 1),
            ),
            (
                "complete-expired".to_string(),
                job("complete-expired", "complete", now - 24 * 60 * 60 - 1),
            ),
            ("running-live".to_string(), live_leased),
        ]);
        for job in jobs.values() {
            repo.create_serve_mapping_proof(job).unwrap();
        }

        assert!(repo
            .load_serve_mapping_proof_header("pending-live")
            .unwrap()
            .is_some());
        assert!(repo
            .load_serve_mapping_proof_header("pending-expired")
            .unwrap()
            .is_some());
        assert!(repo
            .load_serve_mapping_proof_header("complete-live")
            .unwrap()
            .is_some());
        assert!(repo
            .load_serve_mapping_proof_header("complete-expired")
            .unwrap()
            .is_none());
        assert!(repo
            .load_serve_mapping_proof_header("running-live")
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_reclaims_expired_running_mapping_proof_and_fences_crashed_worker() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let bytes = b"resume after crash";
        let hash = hash_bytes(bytes);
        repo.store_chunk(&hash, bytes).unwrap();
        let chunk = BlobProofChunk {
            hash: hash.to_string(),
            offset: 0,
            size: bytes.len() as u32,
        };
        let now = Utc::now().timestamp();
        repo.create_serve_mapping_proof(&ServeMappingProofJob {
            token: "crashed-proof".to_string(),
            request_digest: "crashed-request".to_string(),
            status: "uploading".to_string(),
            worker_token: None,
            lease_expires_at: None,
            created_at: now - 300,
            updated_at: now - 300,
            descriptors: vec![BlobProofDescriptor {
                hash: hash.to_string(),
                size: bytes.len() as u64,
                mapping_digest: oak_core::protocol::blob_mapping_digest(std::slice::from_ref(
                    &chunk,
                )),
                total_chunks: 1,
            }],
            base_mapping_digests: vec![None],
            mappings: vec![BTreeMap::new()],
            verified: Vec::new(),
            missing: Vec::new(),
        })
        .unwrap();
        repo.store_serve_mapping_proof_pages(
            "crashed-proof",
            &[oak_core::protocol::BlobProofMappingPage {
                blob_index: 0,
                first_chunk_index: 0,
                chunks: vec![chunk],
            }],
            now - 300,
        )
        .unwrap();
        assert!(repo
            .claim_serve_mapping_proof("crashed-proof", "dead-worker", now - 200, now - 1)
            .unwrap());
        drop(repo);

        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/blobs/proofs/crashed-proof");
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let response = test_client().get(&endpoint).send().await.unwrap();
                if response.status() == StatusCode::OK {
                    break response.json::<BlobProofResponse>().await.unwrap();
                }
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup worker must reclaim the expired lease");
        assert_eq!(terminal.verified, vec![hash.to_string()]);
        assert_eq!(terminal.proof_token.as_deref(), Some("crashed-proof"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mapping_proof_create_accepts_exact_set_bounds_and_rejects_one_more_ref() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let request = |hash: String, total_chunks: u32| BlobProofRequest {
            blobs: vec![BlobProofDescriptor {
                hash,
                size: oak_core::protocol::MAPPING_PROOF_MAX_SET_BYTES,
                mapping_digest: "b".repeat(64),
                total_chunks,
            }],
        };

        let exact = test_client()
            .post(format!("{base}/api/oak/oak/blobs/proofs/async-v1"))
            .json(&request(
                "a".repeat(64),
                oak_core::protocol::MAPPING_PROOF_MAX_SET_CHUNK_REFS as u32,
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(exact.status(), StatusCode::ACCEPTED);

        let over = test_client()
            .post(format!("{base}/api/oak/oak/blobs/proofs/async-v1"))
            .json(&request(
                "c".repeat(64),
                oak_core::protocol::MAPPING_PROOF_MAX_SET_CHUNK_REFS as u32 + 1,
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(over.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mapping_proof_finalize_body_limit_is_exactly_four_kibibytes() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/blobs/proofs/unknown/finalize");
        let exact = format!(
            "{{}}{}",
            " ".repeat(oak_core::protocol::MAPPING_PROOF_FINALIZE_BODY_BYTES - 2)
        );
        let accepted = test_client()
            .post(&endpoint)
            .header("content-type", "application/json")
            .body(exact)
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::NOT_FOUND);

        let over = format!(
            "{{}}{}",
            " ".repeat(oak_core::protocol::MAPPING_PROOF_FINALIZE_BODY_BYTES - 1)
        );
        let rejected = test_client()
            .post(&endpoint)
            .header("content-type", "application/json")
            .body(over)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_push_rejects_alternate_mapping_for_published_blob() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let bytes = b"abcdef".to_vec();
        let blob_hash = hash_bytes(&bytes);
        let tree = Tree::new(vec![oak_core::TreeEntry {
            name: "data.bin".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        let publish = PushRequest {
            expected_head: None,
            expected_branch_head: None,
            force: false,
            branch: Some(staged_request(Vec::new(), Vec::new(), Vec::new()).branch),
            commits: vec![commit_to_wire(&commit)],
            blobs: vec![BlobData {
                hash: blob_hash.to_string(),
                content: bytes.clone(),
                size: bytes.len() as u64,
                chunks: Vec::new(),
                mapping_proof_token: None,
            }],
            trees: vec![tree_to_wire(&tree)],
        };
        let response: PushResponse = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&publish)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);

        let first = bytes[..3].to_vec();
        let second = bytes[3..].to_vec();
        let first_hash = hash_bytes(&first);
        let second_hash = hash_bytes(&second);
        for (hash, content) in [(&first_hash, &first), (&second_hash, &second)] {
            let response = test_client()
                .put(format!("{base}/api/oak/oak/chunks/{hash}"))
                .body(content.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let mut alternate = staged_request(
            Vec::new(),
            Vec::new(),
            vec![BlobData {
                hash: blob_hash.to_string(),
                content: Vec::new(),
                size: bytes.len() as u64,
                chunks: vec![
                    ChunkRefData {
                        hash: first_hash.to_string(),
                        offset: 0,
                        size: first.len() as u32,
                    },
                    ChunkRefData {
                        hash: second_hash.to_string(),
                        offset: first.len() as u64,
                        size: second.len() as u32,
                    },
                ],
                mapping_proof_token: None,
            }],
        );
        alternate.expected_branch_head = Some(commit.hash.to_string());
        let response = test_client()
            .post(format!("{base}/api/oak/oak/push/staged-v1"))
            .json(&alternate)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let check: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![blob_hash.to_string()],
                require_verified_receipts: false,
                verify_content: true,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(check.missing.is_empty());
        assert!(check.verified_content);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_abort_is_boundary_bound_and_idempotent_within_replay_window() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let bytes = b"abandoned".to_vec();
        let blob = hash_bytes(&bytes);
        let stage = staged_request(
            Vec::new(),
            Vec::new(),
            vec![BlobData {
                hash: blob.to_string(),
                content: bytes,
                size: 9,
                chunks: Vec::new(),
                mapping_proof_token: None,
            }],
        );
        let response = test_client()
            .post(&endpoint)
            .json(&stage)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let abort_url = format!("{endpoint}/{}/abort", stage.stage_id);
        let abort = StagedAbortRequest {
            branch_name: stage.branch.name.clone(),
            expected_branch_head: stage.expected_branch_head.clone(),
        };
        for _ in 0..2 {
            let response: StagedAbortResponse = test_client()
                .post(&abort_url)
                .json(&abort)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(response.aborted);
            assert_eq!(response.state, "aborted");
        }
        let tombstone = load_active_stage_sessions(&repo)
            .unwrap()
            .remove(&stage.stage_id)
            .unwrap();
        assert_eq!(tombstone.state, ServeStageState::Aborted);
        assert!(tombstone.commits.is_empty());
        assert!(tombstone.trees.is_empty());
        assert!(tombstone.blobs.is_empty());
        assert!(tombstone.blob_mappings.is_empty());
        assert!(tombstone.chunks.is_empty());
        assert_eq!(tombstone.totals, ServeStageTotals::default());
        assert!(repo.get_branch_head("main").unwrap().is_none());

        let retry = test_client()
            .post(&endpoint)
            .json(&stage)
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::CONFLICT);
        let hidden: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![blob.to_string()],
                require_verified_receipts: false,
                verify_content: false,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hidden.missing, vec![blob.to_string()]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finalization_rejects_unreachable_session_objects_without_publishing_them() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let bytes = b"orphan".to_vec();
        let orphan_blob = hash_bytes(&bytes);
        let stage = staged_request(
            Vec::new(),
            Vec::new(),
            vec![BlobData {
                hash: orphan_blob.to_string(),
                content: bytes,
                size: 6,
                chunks: Vec::new(),
                mapping_proof_token: None,
            }],
        );
        assert_eq!(
            test_client()
                .post(&endpoint)
                .json(&stage)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        let mut commit_stage = staged_request(vec![commit_to_wire(&commit)], vec![], vec![]);
        commit_stage.stage_id = stage.stage_id.clone();
        assert_eq!(
            test_client()
                .post(&endpoint)
                .json(&commit_stage)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let mut finalize = staged_request(Vec::new(), Vec::new(), Vec::new());
        finalize.stage_id = stage.stage_id;
        finalize.finalize = true;
        finalize.target_head = Some(commit.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&finalize)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(repo.get_branch_head("main").unwrap().is_none());
        let hidden: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![orphan_blob.to_string()],
                require_verified_receipts: false,
                verify_content: false,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hidden.missing, vec![orphan_blob.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn staged_v1_rejects_incomplete_closure_and_finalizes_once() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let timestamp = Utc::now();

        let missing_parent = Hash("ab".repeat(32));
        let orphan = Commit::with_timestamp(
            "main".to_string(),
            Some(missing_parent),
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp,
        )
        .unwrap();
        let response = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&orphan)],
                vec![],
                vec![],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&orphan.hash).unwrap());

        let missing_tree_hash = Hash("cd".repeat(32));
        let missing_tree_commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            missing_tree_hash,
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(1),
        )
        .unwrap();
        let response = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&missing_tree_commit)],
                vec![],
                vec![],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&missing_tree_commit.hash).unwrap());

        let absent_blob = Hash("ef".repeat(32));
        let blob_tree = Tree::new(vec![oak_core::TreeEntry {
            name: "missing.bin".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: absent_blob,
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let missing_blob_commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            blob_tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(2),
        )
        .unwrap();
        let response = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&missing_blob_commit)],
                vec![tree_to_wire(&blob_tree)],
                vec![],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&missing_blob_commit.hash).unwrap());
        assert!(repo.get_tree(&blob_tree.hash).unwrap().is_none());

        let chunk_bytes = b"abc".to_vec();
        let chunk_hash = hash_bytes(&chunk_bytes);
        repo.store_chunk(&chunk_hash, &chunk_bytes).unwrap();
        let malformed_tree = Tree::new(vec![oak_core::TreeEntry {
            name: "bad.bin".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: chunk_hash.clone(),
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let malformed_commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            malformed_tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(3),
        )
        .unwrap();
        let response = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&malformed_commit)],
                vec![tree_to_wire(&malformed_tree)],
                vec![BlobData {
                    hash: chunk_hash.to_string(),
                    content: Vec::new(),
                    size: 3,
                    chunks: vec![ChunkRefData {
                        hash: chunk_hash.to_string(),
                        offset: 1,
                        size: 3,
                    }],
                    mapping_proof_token: None,
                }],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&malformed_commit.hash).unwrap());
        assert!(repo.get_tree(&malformed_tree.hash).unwrap().is_none());
        assert!(repo.get_blob(&chunk_hash).unwrap().is_none());

        let staged_bytes = b"pre-staged closure".to_vec();
        let staged_blob = hash_bytes(&staged_bytes);
        let staged_tree = Tree::new(vec![oak_core::TreeEntry {
            name: "ready.txt".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: staged_blob.clone(),
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![],
                vec![],
                vec![BlobData {
                    hash: staged_blob.to_string(),
                    content: staged_bytes.clone(),
                    size: staged_bytes.len() as u64,
                    chunks: vec![],
                    mapping_proof_token: None,
                }],
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![],
                vec![tree_to_wire(&staged_tree)],
                vec![],
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);
        assert!(repo.get_branch("main").unwrap().is_none());
        assert!(repo.get_branch_head("main").unwrap().is_none());

        let staged = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            staged_tree.hash,
            "tester".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(4),
        )
        .unwrap();
        let unrelated = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "other".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(5),
        )
        .unwrap();
        repo.store_commit(&unrelated).unwrap();
        let mut premature_target = staged_request(vec![commit_to_wire(&staged)], vec![], vec![]);
        premature_target.target_head = Some(staged.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&premature_target)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&staged.hash).unwrap());

        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&staged)],
                vec![],
                vec![],
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);
        assert!(repo.get_branch("main").unwrap().is_none());
        assert!(repo.get_branch_head("main").unwrap().is_none());

        let hidden_commits: CommitInfoResponse = test_client()
            .post(format!("{base}/api/oak/oak/commits/info"))
            .json(&CommitInfoRequest {
                hashes: vec![staged.hash.to_string()],
                metadata_only: true,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            hidden_commits.commits.is_empty(),
            "staged commits must not be visible through hash-addressed reads"
        );
        let hidden_blobs: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![staged_blob.to_string()],
                require_verified_receipts: false,
                verify_content: false,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hidden_blobs.missing, vec![staged_blob.to_string()]);
        let hidden_chunks: ChunkDownloadResponse = test_client()
            .post(format!("{base}/api/oak/oak/chunks/download"))
            .json(&ChunkDownloadRequest {
                hashes: vec![staged_blob.to_string()],
                chunk_batch_protocol: Some(oak_core::protocol::CHUNK_BATCH_PROTOCOL.to_string()),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            hidden_chunks.chunks.is_empty(),
            "staged chunks must not be visible before atomic finalization"
        );
        let hidden_pull: PullResponse = test_client()
            .get(format!("{base}/api/oak/oak/pull?branch_name=main"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            hidden_pull.commits.is_empty(),
            "branch pull must not expose commits before atomic finalization"
        );

        let mut unrelated_final = staged_request(vec![], vec![], vec![]);
        unrelated_final.finalize = true;
        unrelated_final.target_head = Some(unrelated.hash.to_string());
        let response = test_client()
            .post(&endpoint)
            .json(&unrelated_final)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(repo.get_branch_head("main").unwrap().is_none());

        let mut final_request = staged_request(vec![], vec![], vec![]);
        final_request.finalize = true;
        final_request.target_head = Some(staged.hash.to_string());
        let first = test_client().post(&endpoint).json(&final_request).send();
        let second = test_client().post(&endpoint).json(&final_request).send();
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let first_body = first.text().await.unwrap();
        let second = second.unwrap();
        let second_body = second.text().await.unwrap();
        let responses = [first_body.as_str(), second_body.as_str()];
        let is_winner = |body: &str| {
            serde_json::from_str::<PushResponse>(body)
                .is_ok_and(|response| response.success && response.message == "ok")
        };
        assert_eq!(
            responses.iter().filter(|body| is_winner(body)).count(),
            1,
            "concurrent finalizers must have exactly one transactional winner"
        );
        let replay: PushResponse = test_client()
            .post(&endpoint)
            .json(&final_request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(replay.success, "the exact lost-response retry must replay");
        assert_eq!(
            repo.get_branch_head("main").unwrap(),
            Some(staged.hash.clone())
        );
        let published_commits: CommitInfoResponse = test_client()
            .post(format!("{base}/api/oak/oak/commits/info"))
            .json(&CommitInfoRequest {
                hashes: vec![staged.hash.to_string()],
                metadata_only: true,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(published_commits.commits.len(), 1);
        let published_blobs: BlobCheckResponse = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![staged_blob.to_string()],
                require_verified_receipts: false,
                verify_content: true,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(published_blobs.missing.is_empty());
        assert!(published_blobs.verified_content);
        let sessions = load_active_stage_sessions(&repo).unwrap();
        let tombstone = sessions.get(&final_request.stage_id).unwrap();
        assert_eq!(tombstone.state, ServeStageState::Completed);
        assert!(tombstone.commits.is_empty());
        assert!(tombstone.trees.is_empty());
        assert!(tombstone.blobs.is_empty());
        assert!(tombstone.chunks.is_empty());
        assert!(tombstone.manifest_roots.is_empty());
        assert_eq!(tombstone.totals, ServeStageTotals::default());
        assert!(serde_json::to_vec(tombstone).unwrap().len() < 512);

        let mut altered_replay = final_request;
        altered_replay.branch.status = "closed".to_string();
        let response = test_client()
            .post(&endpoint)
            .json(&altered_replay)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_sessions_are_isolated_and_finalization_clears_only_its_owner() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let timestamp = Utc::now();
        let a = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "a".to_string(),
            None,
            Vec::new(),
            timestamp,
        )
        .unwrap();
        let b = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "b".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(1),
        )
        .unwrap();
        let steals_a = Commit::with_timestamp(
            "main".to_string(),
            Some(a.hash.clone()),
            None,
            Tree::empty_hash(),
            "b".to_string(),
            None,
            Vec::new(),
            timestamp + chrono::Duration::seconds(2),
        )
        .unwrap();

        let mut stage_a = staged_request(vec![commit_to_wire(&a)], vec![], vec![]);
        stage_a.stage_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&stage_a)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);

        let mut cross_session = staged_request(vec![commit_to_wire(&steals_a)], vec![], vec![]);
        cross_session.stage_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let response = test_client()
            .post(&endpoint)
            .json(&cross_session)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&steals_a.hash).unwrap());

        let mut stage_b = staged_request(vec![commit_to_wire(&b)], vec![], vec![]);
        stage_b.stage_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&stage_b)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);

        let mut finalize_a = staged_request(vec![], vec![], vec![]);
        finalize_a.stage_id = stage_a.stage_id.clone();
        finalize_a.finalize = true;
        finalize_a.target_head = Some(a.hash.to_string());
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&finalize_a)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);

        let sessions = load_active_stage_sessions(&repo).unwrap();
        assert_eq!(
            sessions.get(&stage_a.stage_id).map(|session| session.state),
            Some(ServeStageState::Completed)
        );
        assert!(sessions.contains_key(&stage_b.stage_id));

        let mut stale_finalize_b = staged_request(vec![], vec![], vec![]);
        stale_finalize_b.stage_id = stage_b.stage_id.clone();
        stale_finalize_b.finalize = true;
        stale_finalize_b.target_head = Some(b.hash.to_string());
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&stale_finalize_b)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!response.success);
        assert_eq!(repo.get_branch_head("main").unwrap(), Some(a.hash));
        assert!(load_active_stage_sessions(&repo)
            .unwrap()
            .contains_key(&stage_b.stage_id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_sessions_enforce_the_per_repository_active_limit() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");

        for index in 0..oak_core::protocol::STAGED_MAX_ACTIVE_SESSIONS_PER_REPO {
            let mut request = staged_request(Vec::new(), Vec::new(), Vec::new());
            request.stage_id = format!("active-session-{index:02}-0123456789abcdef");
            let response = test_client()
                .post(&endpoint)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let mut overflow = staged_request(Vec::new(), Vec::new(), Vec::new());
        overflow.stage_id = "active-session-overflow-0123456789abcdef".to_string();
        let response = test_client()
            .post(&endpoint)
            .json(&overflow)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn staged_commit_file_modes_must_match_manifest_with_zero_writes() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;
        let bytes = b"mode semantics".to_vec();
        let blob_hash = hash_bytes(&bytes);
        let tree = Tree::new(vec![oak_core::TreeEntry {
            name: "tool".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            tree.hash.clone(),
            "tester".to_string(),
            None,
            vec![oak_core::FileChange {
                path: "tool".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: Some(oak_core::FileMode::Executable),
            }],
            Utc::now(),
        )
        .unwrap();
        let response = test_client()
            .post(format!("{base}/api/oak/oak/push/staged-v1"))
            .json(&staged_request(
                vec![commit_to_wire(&commit)],
                vec![tree_to_wire(&tree)],
                vec![BlobData {
                    hash: blob_hash.to_string(),
                    content: bytes,
                    size: 14,
                    chunks: Vec::new(),
                    mapping_proof_token: None,
                }],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!repo.has_commit(&commit.hash).unwrap());
        assert!(repo.get_tree(&tree.hash).unwrap().is_none());
        assert!(repo.get_blob(&blob_hash).unwrap().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_object_only_session_revokes_trust_without_deleting_immutable_bytes() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;
        let endpoint = format!("{base}/api/oak/oak/push/staged-v1");
        let bytes = b"expired session".to_vec();
        let blob_hash = hash_bytes(&bytes);
        let tree = Tree::new(vec![oak_core::TreeEntry {
            name: "orphan.txt".to_string(),
            kind: oak_core::TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: oak_core::FileMode::Regular,
        }])
        .unwrap();
        let request = staged_request(
            Vec::new(),
            vec![tree_to_wire(&tree)],
            vec![BlobData {
                hash: blob_hash.to_string(),
                content: bytes,
                size: 15,
                chunks: Vec::new(),
                mapping_proof_token: None,
            }],
        );
        let response: PushResponse = test_client()
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success);

        let mut sessions = load_active_stage_sessions(&repo).unwrap();
        sessions.get_mut(&request.stage_id).unwrap().updated_at =
            Utc::now().timestamp() - STAGED_SESSION_TTL_SECS - 1;
        store_stage_sessions(&repo, &sessions).unwrap();

        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        let response = test_client()
            .post(&endpoint)
            .json(&staged_request(
                vec![commit_to_wire(&commit)],
                Vec::new(),
                Vec::new(),
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(load_active_stage_sessions(&repo).unwrap().is_empty());
        assert!(repo.get_tree(&tree.hash).unwrap().is_some());
        assert!(verify_stored_blob(&repo, &blob_hash).unwrap());
        assert!(!repo.has_commit(&commit.hash).unwrap());
    }

    #[test]
    fn published_closure_scan_is_persisted_and_not_repeated_per_stage() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            Utc::now(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        repo.set_branch_head("main", &commit.hash).unwrap();

        let first = published_object_closure(&repo).unwrap();
        assert!(first.commits.contains(commit.hash.as_str()));
        assert!(repo
            .get_metadata(MetadataKey::ServePublishedClosure)
            .unwrap()
            .is_some());

        // Removing the row makes a rescan fail. A second lookup still
        // succeeds from the persisted monotonic cache, proving staging does
        // not repeatedly charge the full published graph.
        let connection =
            rusqlite::Connection::open(repo_db_path(&root, "oak", "oak").unwrap()).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        connection
            .execute(
                "UPDATE branch_heads SET head_hash = ?1 WHERE branch_name = 'main'",
                rusqlite::params!["ab".repeat(32)],
            )
            .unwrap();
        drop(connection);
        let cached = published_object_closure(&repo).unwrap();
        assert!(cached.commits.contains(commit.hash.as_str()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn auxiliary_endpoints_enforce_hosted_list_caps() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root).await;
        let hash = Blob::empty_hash().to_string();
        let response = test_client()
            .post(format!("{base}/api/oak/oak/blobs/check"))
            .json(&BlobCheckRequest {
                hashes: vec![hash.clone(); 33],
                require_verified_receipts: false,
                verify_content: true,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/check"))
            .json(&ChunkCheckRequest {
                hashes: vec![hash.clone(); 10_001],
                sizes: Some(vec![0; 10_001]),
                chunk_batch_protocol: Some("bounded_v1".to_string()),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Compatibility rollout: only clients declaring bounded_v1 opt in to
        // the new cap. Deployed clients omit the append-only field and retain
        // the legacy endpoint behavior while they are upgraded.
        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/check"))
            .json(&ChunkCheckRequest {
                hashes: vec![hash.clone(); 40_000],
                sizes: None,
                chunk_batch_protocol: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/download"))
            .json(&ChunkDownloadRequest {
                hashes: vec![hash.clone(); 10_001],
                chunk_batch_protocol: Some("bounded_v1".to_string()),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/download"))
            .json(&ChunkDownloadRequest {
                hashes: vec![hash.clone(); 40_000],
                chunk_batch_protocol: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/uploaded"))
            .json(&ChunkUploadedRequest {
                hashes: (0..257)
                    .map(|_| oak_core::protocol::ChunkUploadedEntry {
                        hash: hash.clone(),
                        size: 0,
                    })
                    .collect(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let mut body = Vec::new();
        for _ in 0..257 {
            body.extend_from_slice(&(hash.len() as u32).to_be_bytes());
            body.extend_from_slice(hash.as_bytes());
            body.extend_from_slice(&0u32.to_be_bytes());
        }
        let response = test_client()
            .post(format!("{base}/api/oak/oak/chunks/batch"))
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_push_keeps_legacy_more_than_one_thousand_commit_compatibility() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;
        let timestamp = Utc::now();
        let mut parent = None;
        let mut commits = Vec::new();
        for index in 0..1_001 {
            let commit = Commit::with_timestamp(
                "feature".to_string(),
                parent,
                None,
                Tree::empty_hash(),
                "legacy".to_string(),
                Some(format!("commit {index}")),
                Vec::new(),
                timestamp + chrono::Duration::milliseconds(index),
            )
            .unwrap();
            parent = Some(commit.hash.clone());
            commits.push(commit_to_wire(&commit));
        }
        let tip = parent.unwrap();
        let request = PushRequest {
            expected_head: None,
            expected_branch_head: None,
            force: false,
            branch: Some(BranchPushData {
                name: "feature".to_string(),
                description: Some("legacy batch".to_string()),
                parent_branch: Some("main".to_string()),
                status: "open".to_string(),
                created_at: timestamp.to_rfc3339(),
                close_reason: None,
            }),
            commits,
            blobs: Vec::new(),
            trees: Vec::new(),
        };
        let response: PushResponse = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.success, "{}", response.message);
        assert_eq!(repo.get_branch_head("feature").unwrap(), Some(tip));
    }

    /// The `oak desc` sync shape: no commits, `expected_head: None`. Must be
    /// accepted (not "Conflict: remote head has changed"), must update the
    /// branch description, and must NOT move the branch head — the hosted
    /// server regressed on both halves of this at one point.
    #[tokio::test(flavor = "current_thread")]
    async fn metadata_only_push_updates_description_without_moving_head() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let head = seed_branch(&root, "feature-x", "old desc");
        let base = spawn_server(root.clone()).await;

        let resp: PushResponse = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&metadata_only_push("feature-x", "new desc"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp.success,
            "metadata-only push rejected: {}",
            resp.message
        );

        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let br = repo.get_branch("feature-x").unwrap().unwrap();
        assert_eq!(br.description.as_deref(), Some("new desc"));
        assert_eq!(
            repo.get_branch_head("feature-x").unwrap(),
            Some(head),
            "a commit-less push must never move a branch head"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_rejects_mismatched_commit_hash() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let base = spawn_server(root.clone()).await;

        let commit = Commit::new(
            "feature-x".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
        )
        .unwrap();
        let mut wire = commit_to_wire(&commit);
        wire.hash = "d".repeat(64);
        let req = PushRequest {
            expected_head: None,
            expected_branch_head: None,
            force: false,
            branch: Some(BranchPushData {
                name: "feature-x".to_string(),
                description: Some("feature".to_string()),
                parent_branch: Some("main".to_string()),
                status: "open".to_string(),
                created_at: Utc::now().to_rfc3339(),
                close_reason: None,
            }),
            commits: vec![wire],
            blobs: Vec::new(),
            trees: Vec::new(),
        };

        let resp = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let err: ErrorResponse = resp.json().await.unwrap();
        assert!(
            err.error.contains("commit hash mismatch"),
            "unexpected error: {}",
            err.error
        );

        assert!(repo.get_commit(&commit.hash).unwrap().is_none());
        assert!(repo.get_branch("feature-x").unwrap().is_none());
    }

    /// `store_branch` is insert-if-absent; the push handler must still apply
    /// a changed description on every push, not only the first.
    #[tokio::test(flavor = "current_thread")]
    async fn repeated_metadata_pushes_upsert_the_description() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        seed_branch(&root, "feature-y", "v1");
        let base = spawn_server(root.clone()).await;

        let client = test_client();
        for desc in ["v2", "v3"] {
            let resp: PushResponse = client
                .post(format!("{base}/api/oak/oak/push"))
                .json(&metadata_only_push("feature-y", desc))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(
                resp.success,
                "push of desc {desc:?} rejected: {}",
                resp.message
            );
        }

        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let br = repo.get_branch("feature-y").unwrap().unwrap();
        assert_eq!(br.description.as_deref(), Some("v3"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metadata_only_push_from_stale_open_client_does_not_reopen_closed_branch() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let head = seed_branch(&root, "feature-z", "old desc");
        {
            let repo = open_for_write(&root, "oak", "oak").unwrap();
            repo.update_branch_status("feature-z", BranchStatus::Closed)
                .unwrap();
        }
        let base = spawn_server(root.clone()).await;

        let resp: PushResponse = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&metadata_only_push(
                "feature-z",
                "new desc from stale client",
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp.success,
            "metadata-only push rejected: {}",
            resp.message
        );

        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let br = repo.get_branch("feature-z").unwrap().unwrap();
        assert_eq!(
            br.description.as_deref(),
            Some("new desc from stale client")
        );
        assert_eq!(
            br.status,
            BranchStatus::Closed,
            "a stale desc sync must not reopen an already closed branch"
        );
        assert_eq!(
            repo.get_branch_head("feature-z").unwrap(),
            Some(head),
            "a stale desc sync must not move a closed branch head"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metadata_only_push_can_close_open_branch() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let head = seed_branch(&root, "feature-close", "old desc");
        let base = spawn_server(root.clone()).await;

        let resp: PushResponse = test_client()
            .post(format!("{base}/api/oak/oak/push"))
            .json(&metadata_only_push_with_status(
                "feature-close",
                "closing desc",
                "closed",
                Some("stale"),
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp.success,
            "metadata-only close rejected: {}",
            resp.message
        );

        let repo = open_for_write(&root, "oak", "oak").unwrap();
        let br = repo.get_branch("feature-close").unwrap().unwrap();
        assert_eq!(br.description.as_deref(), Some("closing desc"));
        assert_eq!(br.status, BranchStatus::Closed);
        assert_eq!(br.close_reason, Some(CloseReason::Stale));
        assert_eq!(
            repo.get_branch_head("feature-close").unwrap(),
            Some(head),
            "a metadata-only close must not move the branch head"
        );
    }
}
