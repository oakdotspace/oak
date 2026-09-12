//! Wire protocol types for the Oak push/pull/clone sync protocol.
//!
//! These are the request/response DTOs exchanged over HTTP between an Oak
//! client (the `oak` CLI) and an Oak server. They are the **single source of
//! truth** for the wire format, shared by every party that speaks it:
//!
//!   * the `oak` CLI (client),
//!   * the hosted `oak-server` (the oak.space product), and
//!   * `oak serve` (the self-hosted, SQLite-backed minimal server).
//!
//! Keeping them here — rather than copied into each crate — means the three
//! implementations cannot silently drift apart, which is the failure mode this
//! module exists to prevent.
//!
//! ## Rules for editing
//!
//! 1. **Every type derives both `Serialize` and `Deserialize`.** A given party
//!    only ever serializes one direction (the client serializes requests and
//!    deserializes responses; a server does the reverse), but because all three
//!    parties live in different crates we derive both on everything so each can
//!    use a type in whichever direction it needs.
//! 2. **Preserve the serde attributes exactly.** `#[serde(default)]` and
//!    `skip_serializing_if` encode forward/backward compatibility with older
//!    clients and servers (the comments note which fields older peers omit).
//!    Changing them breaks wire compatibility with already-deployed CLIs.
//! 3. **These DTOs are distinct from the domain model** (`Blob`, `Commit`,
//!    `Tree`, …). Don't merge them — they serialize differently. The
//!    [`tree_data_to_core`] / [`tree_to_wire`] helpers convert between the two.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    hash_bytes, Blob, ChangeType, ChunkInfo, Commit, FileChange, FileMode, Hash, Tree, TreeEntry,
    TreeEntryKind,
};

pub const STAGED_ENVELOPE_MAX_COMMITS: usize = 500;
pub const STAGED_OPERATION_MAX_COMMITS: usize = 100_000;
pub const STAGED_MAX_TREE_OBJECTS: usize = 100_000;
pub const STAGED_MAX_DIRECT_TREE_ENTRIES: usize = 1_000_000;
pub const STAGED_MAX_RESOLVED_MANIFEST_ENTRIES: usize = 1_000_000;
pub const STAGED_MAX_FILE_CHANGES: usize = 250_000;
pub const STAGED_MAX_CANONICAL_METADATA_BYTES: usize = 64 * 1024 * 1024;
pub const STAGED_MAX_EXPANDED_PATH_BYTES: usize = 64 * 1024 * 1024;
pub const STAGED_MAX_CHUNK_REFS: usize = 1_000_000;
pub const STAGED_MAX_BLOBS: usize = 50_000;
pub const STAGED_MAX_DECLARED_BLOB_BYTES: u64 = 25 * 1024 * 1024 * 1024;
pub const STAGED_MAX_ACTIVE_SESSIONS_PER_REPO: usize = 8;
pub const STAGED_ACTIVE_SESSION_TTL_SECS: i64 = 24 * 60 * 60;
pub const STAGED_COMPLETED_TOMBSTONE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const STAGED_ABORT_PROTOCOL: &str = "v1";
pub const STAGED_ABORTED_STATE: &str = "aborted";
pub const KNOWN_LOSS_PROTOCOL: &str = "report_v1";
pub const MAPPING_PROOF_PROTOCOL: &str = "async_v1";
pub const ORDINARY_BOOTSTRAP_PROTOCOL: &str = "headless_preload_v1";
pub const MAPPING_PROOF_MAX_BLOBS: usize = 128;
pub const MAPPING_PROOF_MAX_SET_CHUNK_REFS: usize = 131_072;
pub const MAPPING_PROOF_MAX_BLOB_CHUNK_REFS: usize = 131_072;
pub const MAPPING_PROOF_MAX_SET_BYTES: u64 = 25 * 1024 * 1024 * 1024;
pub const MAPPING_PROOF_MAX_BLOB_BYTES: u64 = 25 * 1024 * 1024 * 1024;
pub const MAPPING_PROOF_MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAPPING_PROOF_PAGE_CHUNK_REFS: usize = 10_000;
pub const MAPPING_PROOF_PAGE_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const MAPPING_PROOF_FINALIZE_BODY_BYTES: usize = 4 * 1024;
pub const MAPPING_PROOF_GENERATION_CONFLICT: &str = "mapping_generation_conflict";

pub fn staged_branch_metadata_bytes(branch: &BranchPushData) -> usize {
    branch
        .name
        .len()
        .saturating_add(branch.description.as_ref().map_or(0, String::len))
        .saturating_add(branch.parent_branch.as_ref().map_or(0, String::len))
        .saturating_add(branch.created_at.len())
        .saturating_add(branch.status.len())
        .saturating_add(branch.close_reason.as_ref().map_or(0, String::len))
}

pub fn staged_commit_metadata_bytes(commit: &CommitData) -> usize {
    commit
        .hash
        .len()
        .saturating_add(commit.branch_name.len())
        .saturating_add(commit.manifest_hash.len())
        .saturating_add(commit.author.len())
        .saturating_add(commit.message.as_ref().map_or(0, String::len))
        .saturating_add(commit.files.iter().fold(0usize, |total, file| {
            total
                .saturating_add(file.path.len())
                .saturating_add(file.old_path.as_ref().map_or(0, String::len))
                .saturating_add(160)
        }))
}

pub fn staged_tree_metadata_bytes(tree: &TreeData) -> usize {
    tree.entries.iter().fold(0usize, |total, entry| {
        total
            .saturating_add(entry.name.len())
            .saturating_add(entry.hash.len())
            .saturating_add(16)
    })
}

pub fn staged_blob_metadata_bytes(blob: &BlobData) -> usize {
    blob.hash.len().saturating_add(16).saturating_add(
        blob.chunks.iter().fold(0usize, |total, chunk| {
            total.saturating_add(chunk.hash.len()).saturating_add(16)
        }),
    )
}

// ---------------------------------------------------------------------------
// Repository metadata
// ---------------------------------------------------------------------------

/// Repository info response.
#[derive(Serialize, Deserialize)]
pub struct RepoResponse {
    pub name: String,
    pub description: Option<String>,
    pub head: Option<String>,
    pub is_public: bool,
    pub owner: Option<String>,
    pub emoji: Option<String>,
    pub updated_at: Option<String>,
}

/// List of repositories response.
#[derive(Serialize, Deserialize)]
pub struct RepoListResponse {
    pub repos: Vec<RepoResponse>,
}

/// Create repository request.
#[derive(Serialize, Deserialize)]
pub struct CreateRepoRequest {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub is_public: bool,
    /// Slug of the organization to create the repo under. When provided, the repo
    /// is owned by the organization and the caller must have write access to it.
    /// Mutually exclusive with personal ownership — if omitted, the repo is
    /// owned by the authenticated user. (Ignored by org-less servers.)
    #[serde(default)]
    pub organization_slug: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared object carriers
// ---------------------------------------------------------------------------

/// Commit data for transfer.
#[derive(Serialize, Deserialize, Clone)]
pub struct CommitData {
    pub hash: String,
    pub branch_name: String,
    pub parent_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_parent_hash: Option<String>,
    pub manifest_hash: String,
    pub author: String,
    /// Nullable on the wire: only present for main-branch squash-merge
    /// commits, where it's the merged branch's description. Every other
    /// commit has `None` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub timestamp: String,
    pub files: Vec<FileChangeData>,
}

/// File change data.
#[derive(Serialize, Deserialize, Clone)]
pub struct FileChangeData {
    pub path: String,
    pub change_type: String,
    pub old_blob_hash: Option<String>,
    pub new_blob_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
}

/// Chunk reference for chunked blob transfer.
#[derive(Serialize, Deserialize, Clone)]
pub struct ChunkRefData {
    pub hash: String,
    pub offset: u64,
    pub size: u32,
}

/// Blob data for transfer.
#[derive(Serialize, Deserialize, Clone)]
pub struct BlobData {
    pub hash: String,
    pub content: Vec<u8>,
    pub size: u64,
    /// Chunk metadata for large/chunked blobs (empty for small blobs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkRefData>,
    /// Opaque proof identity for the exact inactive mapping generation this
    /// staged blob is asking the server to activate. Ordinary/legacy blobs
    /// omit it, so deployed readers remain wire-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_proof_token: Option<String>,
}

/// Tree object for wire transfer. One row per directory; entries are the
/// tree's direct children (sub-trees + blobs). The receiver stores via
/// `store_tree`, which computes the hash and persists the entries.
#[derive(Serialize, Deserialize, Clone)]
pub struct TreeData {
    pub hash: String,
    pub entries: Vec<TreeEntryData>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TreeEntryData {
    pub name: String,
    /// "tree" or "blob".
    pub kind: String,
    /// Hash of the child tree (when kind=="tree") or blob (when kind=="blob").
    pub hash: String,
    /// "regular" | "executable" | "symlink" for blobs; "tree" for trees.
    pub mode: String,
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// Push request.
#[derive(Serialize, Deserialize)]
pub struct PushRequest {
    pub expected_head: Option<String>,
    /// The client's last-known head for the branch being pushed. When set,
    /// the server uses this for conflict detection on the branch (instead of
    /// or in addition to the global `expected_head`). Older clients omit
    /// this field; the server falls back to the legacy global-head check.
    #[serde(default)]
    pub expected_branch_head: Option<String>,
    #[serde(default)]
    pub force: bool,
    pub branch: Option<BranchPushData>,
    pub commits: Vec<CommitData>,
    pub blobs: Vec<BlobData>,
    /// Tree objects reachable from each commit's root tree. Replaces the
    /// pre-trees `manifests` field. Sent in arbitrary order — no FK between
    /// trees, so the server can store them as a batch.
    #[serde(default)]
    pub trees: Vec<TreeData>,
}

/// Versioned staged-push request. It intentionally has a distinct endpoint
/// and DTO so a mixed deployment can never send "do not advance" semantics
/// to an old `/push` handler that ignores unknown fields and moves a head.
#[derive(Serialize, Deserialize)]
pub struct StagedPushRequest {
    /// Opaque operation-scoped session identifier. Every batch and the final
    /// CAS for one logical push must reuse the same value.
    pub stage_id: String,
    /// Exact branch head observed by the whole-operation planner. This field
    /// is required on the wire; JSON null explicitly means "expect no head".
    #[serde(deserialize_with = "deserialize_required_optional_string")]
    pub expected_branch_head: Option<String>,
    pub branch: BranchPushData,
    /// False publishes a fully validated immutable closure without visible
    /// branch metadata/head changes. True performs the final atomic CAS.
    pub finalize: bool,
    /// Permit a deliberate non-descendant branch target. This never bypasses
    /// the expected-head CAS, stage ownership, or closure/receipt validation.
    #[serde(default)]
    pub force: bool,
    /// Required for finalization and forbidden for staging.
    pub target_head: Option<String>,
    pub commits: Vec<CommitData>,
    pub blobs: Vec<BlobData>,
    #[serde(default)]
    pub trees: Vec<TreeData>,
}

/// Best-effort release of an unfinished staged operation. The path supplies
/// the opaque stage id; these fields bind the abort to the same immutable
/// branch boundary as every staged batch.
#[derive(Serialize, Deserialize)]
pub struct StagedAbortRequest {
    pub branch_name: String,
    #[serde(deserialize_with = "deserialize_required_optional_string")]
    pub expected_branch_head: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct StagedAbortResponse {
    pub aborted: bool,
    pub state: String,
}

pub fn validate_stage_id(stage_id: &str) -> Result<(), String> {
    if !(16..=128).contains(&stage_id.len())
        || !stage_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "stage_id must be 16..=128 ASCII alphanumeric, '-' or '_' characters".to_string(),
        );
    }
    Ok(())
}

fn deserialize_required_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

/// Branch push data (branch-like container metadata).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct BranchPushData {
    pub name: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

/// Push response.
#[derive(Serialize, Deserialize)]
pub struct PushResponse {
    pub success: bool,
    pub new_head: Option<String>,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/// Pull query parameters.
#[derive(Serialize, Deserialize)]
pub struct PullQuery {
    pub since: Option<String>,
    pub branch_name: Option<String>,
    /// Force pull: skip conflict check and return all commits from remote HEAD.
    #[serde(default)]
    pub force: bool,
    /// Highest branch_renames.id the client has already applied. Server
    /// returns events with id > since_rename_id so the client can replay
    /// any renames performed by other clones since its last pull.
    #[serde(default)]
    pub since_rename_id: i64,
    /// Shallow-clone depth. When set (and no `branch_name` is given — i.e. the
    /// clone case), the server returns only the most recent `depth` commits on
    /// the default branch instead of the full reachable history. `oak clone`
    /// fetches the full history by default (no `depth`); `oak clone --shallow`
    /// sends `depth=1`. Ignored on branch-scoped pulls. A value of `0` is
    /// treated as "no limit" (same as unset).
    pub depth: Option<u32>,
    /// Explicit opt-in to structured operator-adjudicated known-loss reports.
    /// Omitted for legacy peers so adding the protocol does not change their
    /// pull behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_loss_protocol: Option<String>,
}

/// Branch pull data (branch-like container metadata).
#[derive(Serialize, Deserialize)]
pub struct BranchPullData {
    pub name: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

/// Branch rename event included in pull responses so other clones can
/// replay renames against their local repos. Ordered by `id` ASC.
#[derive(Serialize, Deserialize)]
pub struct BranchRenameData {
    pub id: i64,
    pub old_name: String,
    pub new_name: String,
    pub renamed_at: String,
}

/// Content deliberately omitted because an operator has adjudicated legacy
/// bytes as permanently unavailable. This is distinct from access-restricted
/// content and from unexpected corruption, both of which retain their existing
/// behavior.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MissingContentData {
    pub kind: String,
    pub hash: String,
    pub reason_code: String,
}

/// Pull response.
#[derive(Serialize, Deserialize)]
pub struct PullResponse {
    pub head: Option<String>,
    pub branch: Option<BranchPullData>,
    pub branches: Vec<BranchPullData>,
    pub commits: Vec<CommitData>,
    pub blobs: Vec<BlobData>,
    /// Tree objects reachable from each returned commit's root tree.
    /// Replaces the pre-trees `manifests` field.
    pub trees: Vec<TreeData>,
    /// Rename events with id > the client's `since_rename_id`. Older
    /// servers omit this field; clients should default it to an empty
    /// list when missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renames: Vec<BranchRenameData>,
    /// Blob hashes whose content was withheld from `blobs` because path-based
    /// permissions deny this caller. Distinguishes "an admin restricted this
    /// path" from "the server is missing data" so clients can skip the files
    /// with an access message instead of failing the pull as corrupt.
    /// Sparse-clone (out-of-cone) withholding is not listed — the client asked
    /// for that itself. Older servers omit the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restricted_blobs: Vec<String>,
    /// Operator-adjudicated historical content that the server cannot return.
    /// Older servers omit the field; clients request this behavior explicitly
    /// through the versioned known-loss protocol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_content: Vec<MissingContentData>,
}

// ---------------------------------------------------------------------------
// Blob existence / metadata
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct BlobCheckRequest {
    pub hashes: Vec<String>,
    /// Require the server to omit mappings that do not already have durable,
    /// generation-matched blob and child-chunk receipts. This is a metadata
    /// predicate: it must not trigger object-store reads or consume live-proof
    /// quota. Older servers ignore this append-only field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub require_verified_receipts: bool,
    /// Ask the server to physically read and content-hash every requested
    /// blob it reports as present. This expensive operation is reserved for
    /// explicit diagnosis/repair; routine push planning uses receipt metadata.
    /// Older servers ignore this append-only field, so explicit verifiers must
    /// require [`BlobCheckResponse::verified_content`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub verify_content: bool,
}

#[derive(Serialize, Deserialize)]
pub struct BlobCheckResponse {
    /// Subset of the request's `hashes` the server does NOT have. The
    /// client should send only these in the push payload.
    pub missing: Vec<String>,
    /// True only when every requested hash omitted from `missing` was
    /// physically read and reproduced its declared content hash and size.
    /// Absent on legacy servers and therefore false/fail-closed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub verified_content: bool,
    /// Acknowledge that the server applied the request's strict durable
    /// receipt predicate. Clients require this on strict requests so a mixed
    /// deployment cannot silently route them to an older replica that ignored
    /// the append-only request field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub verified_receipts_required: bool,
}

/// Exact mapping headers submitted before paged chunk metadata. Keeping the
/// mapping itself out of this request lets a 25 GiB blob use bounded requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofRequest {
    pub blobs: Vec<BlobProofDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofDescriptor {
    pub hash: String,
    pub size: u64,
    /// Hash of the canonical ordered `(hash, offset, size)` chunk sequence.
    pub mapping_digest: String,
    pub total_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofChunk {
    pub hash: String,
    pub offset: u64,
    pub size: u32,
}

/// Stable identity of one ordered blob-to-chunk layout. This deliberately
/// matches Oakspace's stored receipt digest byte-for-byte; changing it requires
/// a new protocol version and a receipt migration.
pub fn blob_mapping_digest(chunks: &[BlobProofChunk]) -> String {
    let mut canonical = Vec::with_capacity(chunks.len().saturating_mul(84));
    for (index, chunk) in chunks.iter().enumerate() {
        canonical.extend_from_slice(&(index as u64).to_be_bytes());
        canonical.extend_from_slice(&chunk.offset.to_be_bytes());
        canonical.extend_from_slice(&chunk.size.to_be_bytes());
        canonical.extend_from_slice(chunk.hash.as_bytes());
    }
    hash_bytes(&canonical).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofPagesRequest {
    pub pages: Vec<BlobProofMappingPage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofMappingPage {
    pub blob_index: u32,
    pub first_chunk_index: u32,
    pub chunks: Vec<BlobProofChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofPagesResponse {
    pub accepted_chunks: u32,
    pub complete_blobs: Vec<u32>,
    pub all_mappings_complete: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofFinalizeRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobProofResponse {
    pub verified: Vec<String>,
    pub missing: Vec<String>,
    /// Opaque identity binding principal, repository, request digest, exact
    /// mapping generation, and its base generation. Required on every
    /// successful proof, including synchronous HTTP 200 responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_proof_job: Option<MappingProofJob>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingProofJob {
    pub token: String,
    pub status: String,
    pub retry_after_ms: u64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize, Deserialize)]
pub struct BlobInfoRequest {
    pub hashes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct BlobInfoResponse {
    /// One entry per hash that the server has. Missing hashes are
    /// silently omitted — callers should diff the returned set against
    /// the requested set to detect unknown hashes.
    pub blobs: Vec<BlobData>,
    /// Requested hashes withheld because path-based permissions deny this
    /// caller their content — as opposed to hashes the server doesn't know.
    /// Older servers omit the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restricted: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct CommitInfoRequest {
    pub hashes: Vec<String>,
    /// Return only verified commit DTOs, without recursively materializing
    /// their tree DAG. Used for small read-only remote existence proofs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub metadata_only: bool,
}

#[derive(Serialize, Deserialize)]
pub struct CommitInfoResponse {
    /// One entry per hash the server has. Missing hashes are omitted.
    pub commits: Vec<CommitData>,
    /// Tree objects reachable from the returned commits' root trees,
    /// deduplicated by hash.
    pub trees: Vec<TreeData>,
}

// ---------------------------------------------------------------------------
// Chunk transfer
// ---------------------------------------------------------------------------

pub const CHUNK_BATCH_PROTOCOL: &str = "bounded_v1";
pub const CHUNK_BATCH_MAX_HASHES: usize = 10_000;

#[derive(Serialize, Deserialize)]
pub struct ChunkCheckRequest {
    pub hashes: Vec<String>,
    /// Sizes of each chunk in `hashes`, in parallel order. When provided, the
    /// server enforces the organization storage quota up front by summing the
    /// sizes of chunks the organization doesn't already have. Older clients
    /// that omit this field skip the up-front check (the server-mediated
    /// `upload_chunk` path still enforces the quota on receipt).
    #[serde(default)]
    pub sizes: Option<Vec<u64>>,
    /// Opt in to the bounded batch contract. New clients page requests to the
    /// advertised limit and send the exact `bounded_v1` token; old clients
    /// omit the field so a compatibility server can retain its legacy limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_batch_protocol: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkUploadInfo {
    pub hash: String,
    /// Presigned PUT URL for direct R2 upload (None if no R2 configured, in
    /// which case the client uploads via the server-mediated `PUT
    /// /chunks/{hash}` path).
    pub upload_url: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkCheckServerResponse {
    pub missing: Vec<ChunkUploadInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkDownloadRequest {
    pub hashes: Vec<String>,
    /// See [`ChunkCheckRequest::chunk_batch_protocol`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_batch_protocol: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkDownloadInfo {
    pub hash: String,
    /// Presigned GET URL for direct R2 download (None if content is inline).
    pub download_url: Option<String>,
    /// Chunk content from DB (only when R2 is not configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkDownloadResponse {
    pub chunks: Vec<ChunkDownloadInfo>,
    /// Batch-download endpoint (CDN Worker) carrying a capability token, set
    /// only when the chunk CDN is configured. Clients may POST a
    /// `{ "hashes": [...] }` body here to fetch many small chunks in one
    /// length-framed response instead of one GET per chunk. `None` → fall back
    /// to per-chunk `download_url`/`content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_url: Option<String>,
    /// Requested chunk hashes withheld because they belong exclusively to
    /// blobs that path-based permissions deny this caller. Older servers omit
    /// the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restricted: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkUploadedRequest {
    pub hashes: Vec<ChunkUploadedEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct ChunkUploadedEntry {
    pub hash: String,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Generic JSON error body.
#[derive(Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ---------------------------------------------------------------------------
// Conversions between wire DTOs and the core domain model
// ---------------------------------------------------------------------------

/// Convert a wire [`TreeData`] to a core [`Tree`]. Validates `kind` and
/// `mode`, plus entry names and hashes — wire data is untrusted, and a name
/// containing the canonical format's `\t`/`\n` separators (or a hash that
/// isn't hex) would corrupt the stored preimage.
///
/// Crucially, this also **verifies the tree's content-address**: blobs are
/// hash-checked before storage, but trees were not — so a malicious or buggy
/// peer (`oak serve`, the pull endpoint) could store a tree under a hash whose
/// entries point at arbitrary blobs, silently breaking the content-addressing
/// invariant. We rebuild the tree through [`Tree::new`] (which canonicalizes
/// entry order and recomputes the hash) and reject any payload whose claimed
/// hash doesn't match the content.
pub fn tree_data_to_core(td: &TreeData) -> Result<Tree, String> {
    let tree_hash = Hash::from_hex(&td.hash).map_err(|e| e.to_string())?;
    let mut entries = Vec::with_capacity(td.entries.len());
    for e in &td.entries {
        crate::tree::validate_entry_name(&e.name).map_err(|err| err.to_string())?;
        let kind = match e.kind.as_str() {
            "tree" => TreeEntryKind::Tree,
            "blob" => TreeEntryKind::Blob,
            other => return Err(format!("invalid tree entry kind: {other}")),
        };
        let mode = match (kind, e.mode.as_str()) {
            (TreeEntryKind::Tree, "tree") => FileMode::Regular,
            (TreeEntryKind::Blob, "regular") => FileMode::Regular,
            (TreeEntryKind::Blob, "executable") => FileMode::Executable,
            (TreeEntryKind::Blob, "symlink") => FileMode::Symlink,
            _ => {
                return Err(format!(
                    "invalid tree entry mode {:?} for kind {}",
                    e.mode,
                    e.kind.as_str()
                ))
            }
        };
        entries.push(TreeEntry {
            name: e.name.clone(),
            kind,
            hash: Hash::from_hex(&e.hash).map_err(|err| err.to_string())?,
            mode,
        });
    }
    // `Tree::new` sorts entries into canonical order and recomputes the hash
    // over the canonical preimage — the same recipe used when the tree was
    // first created. If the recomputed hash differs from what the peer claimed,
    // the content doesn't match its address: reject it.
    let tree = Tree::new(entries).map_err(|e| e.to_string())?;
    if tree.hash != tree_hash {
        return Err(format!(
            "tree hash mismatch: peer claimed {}, content hashes to {}",
            td.hash, tree.hash
        ));
    }
    Ok(tree)
}

fn wire_hash(field: &str, value: &str) -> Result<Hash, String> {
    Hash::from_hex(value).map_err(|e| format!("invalid {field} hash {value:?}: {e}"))
}

fn wire_optional_hash(field: &str, value: Option<&String>) -> Result<Option<Hash>, String> {
    value.map(|s| wire_hash(field, s)).transpose()
}

fn file_change_data_to_core(f: &FileChangeData, index: usize) -> Result<FileChange, String> {
    crate::tree::validate_tree_path(&f.path)
        .map_err(|e| format!("invalid file change #{index} path: {e}"))?;
    if let Some(old_path) = &f.old_path {
        crate::tree::validate_tree_path(old_path)
            .map_err(|e| format!("invalid file change #{index} old_path: {e}"))?;
    }

    let change_type = match f.change_type.as_str() {
        "added" => ChangeType::Added,
        "modified" => ChangeType::Modified,
        "deleted" => ChangeType::Deleted,
        "renamed" => ChangeType::Renamed,
        other => return Err(format!("invalid file change #{index} type: {other}")),
    };
    let parse_mode = |field: &str, mode: Option<&String>| {
        mode.map(|mode| match mode.as_str() {
            "regular" => Ok(FileMode::Regular),
            "executable" => Ok(FileMode::Executable),
            "symlink" => Ok(FileMode::Symlink),
            other => Err(format!("invalid file change #{index} {field}: {other}")),
        })
        .transpose()
    };

    Ok(FileChange {
        path: f.path.clone(),
        change_type,
        old_blob_hash: wire_optional_hash("old blob", f.old_blob_hash.as_ref())?,
        new_blob_hash: wire_optional_hash("new blob", f.new_blob_hash.as_ref())?,
        old_path: f.old_path.clone(),
        old_mode: parse_mode("old_mode", f.old_mode.as_ref())?,
        new_mode: parse_mode("new_mode", f.new_mode.as_ref())?,
    })
}

pub fn file_mode_to_wire(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "regular",
        FileMode::Executable => "executable",
        FileMode::Symlink => "symlink",
    }
}

/// Convert a wire [`CommitData`] to a core [`Commit`].
///
/// The wire claim is untrusted: validate every hash-position string, validate
/// commit fields through [`Commit::rehydrate_verified`], recompute the current
/// Oak commit hash from canonical fields, and reject any payload whose claimed
/// hash cannot be reproduced from them.
pub fn commit_data_to_core(cd: &CommitData) -> Result<Commit, String> {
    let claimed_hash = wire_hash("commit", &cd.hash)?;
    let parent_hash = wire_optional_hash("parent", cd.parent_hash.as_ref())?;
    let merge_parent_hash = wire_optional_hash("merge parent", cd.merge_parent_hash.as_ref())?;
    let manifest_hash = wire_hash("manifest", &cd.manifest_hash)?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(&cd.timestamp)
        .map_err(|e| format!("invalid commit timestamp {:?}: {e}", cd.timestamp))?
        .with_timezone(&chrono::Utc);
    let files: Vec<FileChange> = cd
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| file_change_data_to_core(f, i))
        .collect::<Result<_, _>>()?;

    // `rehydrate_verified` (rather than a plain recompute-and-compare)
    // tolerates microsecond-truncated timestamps on commits that hashed
    // nanosecond ones — every squash-merge the server stored before
    // `Commit::new` started truncating. See its docs.
    Commit::rehydrate_verified(
        &claimed_hash,
        cd.branch_name.clone(),
        parent_hash,
        merge_parent_hash,
        manifest_hash,
        cd.author.clone(),
        cd.message.clone(),
        files,
        timestamp,
    )
    .map_err(|e| e.to_string())
}

/// Fully parsed staged blob plus the exact chunk mapping that may be stored.
pub struct ValidatedStagedBlob {
    pub blob: Blob,
    pub chunks: Vec<ChunkInfo>,
    /// Inline payloads need their canonical one-chunk object persisted before
    /// the mapping. Chunked payloads are already present and leave this empty.
    pub inline_chunk: Option<(Hash, Vec<u8>)>,
}

/// A staged request admitted without writing anything. Servers should perform
/// this whole validation pass first, then persist these values in one atomic
/// transaction so every rejection is zero-write.
pub struct ValidatedStagedClosure {
    pub commits: Vec<Commit>,
    pub trees: Vec<Tree>,
    pub blobs: Vec<ValidatedStagedBlob>,
    pub resolved_tree_entries: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn validate_staged_closure<FC, FT, FB, FH>(
    req: &StagedPushRequest,
    mut commit_exists: FC,
    mut get_tree: FT,
    mut verify_blob: FB,
    mut get_chunk: FH,
) -> Result<ValidatedStagedClosure, String>
where
    FC: FnMut(&Hash) -> Result<bool, String>,
    FT: FnMut(&Hash) -> Result<Option<Tree>, String>,
    FB: FnMut(&Hash) -> Result<bool, String>,
    FH: FnMut(&Hash) -> Result<Option<Vec<u8>>, String>,
{
    validate_stage_id(&req.stage_id)?;
    if req.finalize {
        return Err("finalization is not an object-staging request".to_string());
    }
    if req.target_head.is_some() {
        return Err("target_head is forbidden when finalize=false".to_string());
    }

    let mut commits = Vec::with_capacity(req.commits.len());
    let mut incoming_commits = HashMap::new();
    for wire in &req.commits {
        let commit = commit_data_to_core(wire)?;
        if incoming_commits
            .insert(commit.hash.clone(), commit.clone())
            .is_some()
        {
            return Err(format!("duplicate incoming commit {}", commit.hash));
        }
        commits.push(commit);
    }
    for commit in &commits {
        for dependency in [&commit.parent_hash, &commit.merge_parent_hash]
            .into_iter()
            .flatten()
        {
            if !incoming_commits.contains_key(dependency) && !commit_exists(dependency)? {
                return Err(format!(
                    "commit {} has missing parent or merge parent {dependency}",
                    commit.hash
                ));
            }
        }
    }

    let mut trees = Vec::with_capacity(req.trees.len());
    let mut incoming_trees = HashMap::new();
    for wire in &req.trees {
        let tree = tree_data_to_core(wire)?;
        if incoming_trees
            .insert(tree.hash.clone(), tree.clone())
            .is_some()
        {
            return Err(format!("duplicate incoming tree {}", tree.hash));
        }
        trees.push(tree);
    }

    let mut blobs = Vec::with_capacity(req.blobs.len());
    let mut incoming_blobs = HashMap::new();
    for wire in &req.blobs {
        let hash = wire_hash("blob", &wire.hash)?;
        if incoming_blobs.contains_key(&hash) {
            return Err(format!("duplicate incoming blob {hash}"));
        }
        let (content, chunks, inline_chunk) = if wire.chunks.is_empty() {
            if wire.size != wire.content.len() as u64 {
                return Err(format!(
                    "inline blob {hash} declares size {} but carries {} bytes",
                    wire.size,
                    wire.content.len()
                ));
            }
            if hash_bytes(&wire.content) != hash {
                return Err(format!("inline blob {hash} content hash mismatch"));
            }
            let chunk = ChunkInfo {
                hash: hash.clone(),
                offset: 0,
                length: wire
                    .content
                    .len()
                    .try_into()
                    .map_err(|_| format!("inline blob {hash} exceeds chunk size range"))?,
            };
            (
                wire.content.clone(),
                vec![chunk],
                Some((hash.clone(), wire.content.clone())),
            )
        } else {
            if !wire.content.is_empty() {
                return Err(format!(
                    "chunked blob {hash} must not also carry inline content"
                ));
            }
            let mut expected_offset = 0u64;
            let mut chunk_infos = Vec::with_capacity(wire.chunks.len());
            let mut hasher = blake3::Hasher::new();
            for chunk in &wire.chunks {
                let chunk_hash = wire_hash("chunk", &chunk.hash)?;
                if chunk.offset != expected_offset {
                    return Err(format!(
                        "blob {hash} chunk {} starts at {}, expected {expected_offset}",
                        chunk.hash, chunk.offset
                    ));
                }
                let bytes = get_chunk(&chunk_hash)?
                    .ok_or_else(|| format!("blob {hash} is missing chunk {chunk_hash}"))?;
                if bytes.len() != chunk.size as usize || hash_bytes(&bytes) != chunk_hash {
                    return Err(format!(
                        "blob {hash} chunk {chunk_hash} size or content hash mismatch"
                    ));
                }
                expected_offset = expected_offset
                    .checked_add(chunk.size as u64)
                    .ok_or_else(|| format!("blob {hash} chunk offsets overflow"))?;
                chunk_infos.push(ChunkInfo {
                    hash: chunk_hash,
                    offset: chunk.offset,
                    length: chunk.size,
                });
                hasher.update(&bytes);
            }
            if expected_offset != wire.size {
                return Err(format!(
                    "blob {hash} chunk chain totals {expected_offset}, declared size {}",
                    wire.size
                ));
            }
            let actual = Hash(hasher.finalize().to_hex().to_string());
            if actual != hash {
                return Err(format!("blob {hash} chunk chain content hash mismatch"));
            }
            (Vec::new(), chunk_infos, None)
        };
        let blob = Blob {
            hash: hash.clone(),
            size: wire.size,
            content,
        };
        incoming_blobs.insert(hash, blobs.len());
        blobs.push(ValidatedStagedBlob {
            blob,
            chunks,
            inline_chunk,
        });
    }

    let object_only = commits.is_empty();
    let mut reached_trees = HashSet::new();
    let mut reached_blobs = HashSet::new();
    let mut resolved_tree_entries = 0usize;
    let mut stack: Vec<Hash> = if object_only {
        // Object-only staging is the bounded prelude for a commit whose
        // closure exceeds one admission envelope. Treat every supplied tree
        // as a root, while still requiring all of its dependencies to be
        // supplied in this request or already durably stored.
        incoming_trees.keys().cloned().collect()
    } else {
        commits
            .iter()
            .map(|commit| commit.manifest_hash.clone())
            .collect()
    };
    while let Some(hash) = stack.pop() {
        if hash == Tree::empty_hash() || !reached_trees.insert(hash.clone()) {
            continue;
        }
        let tree = match incoming_trees.get(&hash) {
            Some(tree) => tree.clone(),
            None => get_tree(&hash)?.ok_or_else(|| format!("missing reachable tree {hash}"))?,
        };
        resolved_tree_entries = resolved_tree_entries
            .checked_add(tree.entries.len())
            .ok_or_else(|| "resolved tree entry count overflow".to_string())?;
        for entry in tree.entries {
            match entry.kind {
                TreeEntryKind::Tree => stack.push(entry.hash),
                TreeEntryKind::Blob => {
                    if !reached_blobs.insert(entry.hash.clone()) {
                        continue;
                    }
                    if !incoming_blobs.contains_key(&entry.hash) && !verify_blob(&entry.hash)? {
                        return Err(format!("missing or corrupt reachable blob {}", entry.hash));
                    }
                }
            }
        }
    }
    if let Some(extra) = incoming_trees
        .keys()
        .find(|hash| !reached_trees.contains(*hash))
    {
        return Err(format!(
            "incoming tree {extra} is not reachable from a commit"
        ));
    }
    if let Some(extra) = incoming_blobs
        .keys()
        .find(|hash| !object_only && !reached_blobs.contains(*hash))
    {
        return Err(format!(
            "incoming blob {extra} is not reachable from a commit"
        ));
    }

    Ok(ValidatedStagedClosure {
        commits,
        trees,
        blobs,
        resolved_tree_entries,
    })
}

/// Test whether `tip` reaches `boundary` through parent or merge-parent edges.
/// `None` is the explicit empty-branch boundary and is always reached.
pub fn commit_reaches_boundary<F>(
    tip: &Hash,
    boundary: Option<&Hash>,
    mut get_commit: F,
) -> Result<bool, String>
where
    F: FnMut(&Hash) -> Result<Option<Commit>, String>,
{
    let Some(boundary) = boundary else {
        return Ok(true);
    };
    let mut seen = HashSet::new();
    let mut stack = vec![tip.clone()];
    while let Some(hash) = stack.pop() {
        if &hash == boundary {
            return Ok(true);
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        let Some(commit) = get_commit(&hash)? else {
            continue;
        };
        if let Some(parent) = commit.parent_hash {
            stack.push(parent);
        }
        if let Some(merge_parent) = commit.merge_parent_hash {
            stack.push(merge_parent);
        }
    }
    Ok(false)
}

/// Convert a core [`Tree`] to wire [`TreeData`].
pub fn tree_to_wire(tree: &Tree) -> TreeData {
    TreeData {
        hash: tree.hash.to_string(),
        entries: tree
            .entries
            .iter()
            .map(|e| TreeEntryData {
                name: e.name.clone(),
                kind: e.kind.as_str().to_string(),
                hash: e.hash.to_string(),
                mode: match (e.kind, e.mode) {
                    (TreeEntryKind::Tree, _) => "tree".to_string(),
                    (TreeEntryKind::Blob, FileMode::Regular) => "regular".to_string(),
                    (TreeEntryKind::Blob, FileMode::Executable) => "executable".to_string(),
                    (TreeEntryKind::Blob, FileMode::Symlink) => "symlink".to_string(),
                },
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_mapping_proof_wire_is_versioned_and_exact() {
        let request = BlobProofRequest {
            blobs: vec![BlobProofDescriptor {
                hash: "a".repeat(64),
                size: 3,
                mapping_digest: "6ed22faf011636d53e22b7f48e5c9ac2ef718887ebc0b7009ddb08476fdced82"
                    .to_string(),
                total_chunks: 1,
            }],
        };
        assert_eq!(MAPPING_PROOF_PROTOCOL, "async_v1");
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "blobs": [{
                    "hash": "a".repeat(64),
                    "size": 3,
                    "mapping_digest": "6ed22faf011636d53e22b7f48e5c9ac2ef718887ebc0b7009ddb08476fdced82",
                    "total_chunks": 1
                }]
            })
        );
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            format!(
                "{{\"blobs\":[{{\"hash\":\"{}\",\"size\":3,\"mapping_digest\":\"6ed22faf011636d53e22b7f48e5c9ac2ef718887ebc0b7009ddb08476fdced82\",\"total_chunks\":1}}]}}",
                "a".repeat(64)
            )
        );

        let created: BlobProofResponse = serde_json::from_value(serde_json::json!({
            "verified": [],
            "missing": [],
            "mapping_proof_job": {
                "token": "proof-token",
                "status": "uploading",
                "retry_after_ms": 500
            }
        }))
        .unwrap();
        assert_eq!(
            created.mapping_proof_job.as_ref().unwrap().token,
            "proof-token"
        );
        assert_eq!(
            serde_json::to_string(&created).unwrap(),
            "{\"verified\":[],\"missing\":[],\"mapping_proof_job\":{\"token\":\"proof-token\",\"status\":\"uploading\",\"retry_after_ms\":500}}"
        );

        let pages = BlobProofPagesRequest {
            pages: vec![BlobProofMappingPage {
                blob_index: 0,
                first_chunk_index: 0,
                chunks: vec![BlobProofChunk {
                    hash: "c".repeat(64),
                    offset: 0,
                    size: 3,
                }],
            }],
        };
        assert_eq!(
            serde_json::to_value(&pages).unwrap(),
            serde_json::json!({
                "pages": [{
                    "blob_index": 0,
                    "first_chunk_index": 0,
                    "chunks": [{
                        "hash": "c".repeat(64),
                        "offset": 0,
                        "size": 3
                    }]
                }]
            })
        );
        assert_eq!(
            serde_json::to_string(&pages).unwrap(),
            format!(
                "{{\"pages\":[{{\"blob_index\":0,\"first_chunk_index\":0,\"chunks\":[{{\"hash\":\"{}\",\"offset\":0,\"size\":3}}]}}]}}",
                "c".repeat(64)
            )
        );

        let accepted: BlobProofPagesResponse = serde_json::from_value(serde_json::json!({
            "accepted_chunks": 1,
            "complete_blobs": [0],
            "all_mappings_complete": true
        }))
        .unwrap();
        assert!(accepted.all_mappings_complete);
        assert_eq!(
            serde_json::to_string(&accepted).unwrap(),
            "{\"accepted_chunks\":1,\"complete_blobs\":[0],\"all_mappings_complete\":true}"
        );

        assert_eq!(
            serde_json::to_value(BlobProofFinalizeRequest {}).unwrap(),
            serde_json::json!({})
        );

        let finalized: BlobProofResponse = serde_json::from_value(serde_json::json!({
            "verified": [],
            "missing": [],
            "mapping_proof_job": {
                "token": "proof-token",
                "status": "pending",
                "retry_after_ms": 500
            }
        }))
        .unwrap();
        assert_eq!(
            finalized.mapping_proof_job.as_ref().unwrap().status,
            "pending"
        );
        assert_eq!(
            serde_json::to_string(&finalized).unwrap(),
            "{\"verified\":[],\"missing\":[],\"mapping_proof_job\":{\"token\":\"proof-token\",\"status\":\"pending\",\"retry_after_ms\":500}}"
        );

        let terminal = serde_json::to_value(BlobProofResponse {
            verified: vec!["a".repeat(64)],
            missing: Vec::new(),
            proof_token: Some("proof-token".to_string()),
            mapping_proof_job: None,
        })
        .unwrap();
        assert_eq!(terminal["proof_token"], "proof-token");
        assert_eq!(
            serde_json::to_string(&BlobProofResponse {
                verified: vec!["a".repeat(64)],
                missing: Vec::new(),
                proof_token: Some("proof-token".to_string()),
                mapping_proof_job: None,
            })
            .unwrap(),
            format!(
                "{{\"verified\":[\"{}\"],\"missing\":[],\"proof_token\":\"proof-token\"}}",
                "a".repeat(64)
            )
        );
    }

    #[test]
    fn mapping_digest_matches_the_stored_receipt_identity() {
        let chunks = vec![
            BlobProofChunk {
                hash: "c".repeat(64),
                offset: 0,
                size: 3,
            },
            BlobProofChunk {
                hash: "c".repeat(64),
                offset: 3,
                size: 3,
            },
        ];

        assert_eq!(
            blob_mapping_digest(&chunks[..1]),
            "6ed22faf011636d53e22b7f48e5c9ac2ef718887ebc0b7009ddb08476fdced82"
        );
        assert_eq!(
            blob_mapping_digest(&chunks),
            "2604417dcfdfa8bc4fc85d2b24a965fb8d7c328d00723c31b75ad7bbdaa194fe"
        );
        assert_eq!(
            blob_mapping_digest(&[]),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn blob_check_verified_content_is_append_only_and_fail_closed() {
        let legacy: BlobCheckResponse =
            serde_json::from_value(serde_json::json!({ "missing": [] })).unwrap();
        assert!(!legacy.verified_content);
        assert!(!legacy.verified_receipts_required);

        let ordinary = serde_json::to_value(BlobCheckRequest {
            hashes: vec!["ab".repeat(32)],
            require_verified_receipts: false,
            verify_content: false,
        })
        .unwrap();
        assert!(ordinary.get("verify_content").is_none());
        assert!(ordinary.get("require_verified_receipts").is_none());

        let receipt_strict = serde_json::to_value(BlobCheckRequest {
            hashes: vec!["ab".repeat(32)],
            require_verified_receipts: true,
            verify_content: false,
        })
        .unwrap();
        assert_eq!(receipt_strict["require_verified_receipts"], true);
        assert!(receipt_strict.get("verify_content").is_none());

        let verified = serde_json::to_value(BlobCheckRequest {
            hashes: vec!["ab".repeat(32)],
            require_verified_receipts: false,
            verify_content: true,
        })
        .unwrap();
        assert_eq!(verified["verify_content"], true);
    }

    #[test]
    fn staged_push_protocol_is_structurally_separate_from_legacy_push() {
        let ordinary: PushRequest = serde_json::from_value(serde_json::json!({
            "expected_head": null,
            "branch": null,
            "commits": [],
            "blobs": [],
            "trees": []
        }))
        .unwrap();
        let ordinary = serde_json::to_value(&ordinary).unwrap();
        assert!(ordinary.get("finalize").is_none());

        let staged = serde_json::to_value(StagedPushRequest {
            stage_id: "0123456789abcdef0123456789abcdef".to_string(),
            expected_branch_head: None,
            branch: BranchPushData {
                name: "main".to_string(),
                description: None,
                parent_branch: None,
                status: "open".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: None,
            },
            finalize: false,
            force: true,
            target_head: None,
            commits: Vec::new(),
            blobs: Vec::new(),
            trees: Vec::new(),
        })
        .unwrap();
        assert_eq!(staged["expected_branch_head"], serde_json::Value::Null);
        assert_eq!(staged["finalize"], false);
        assert_eq!(staged["force"], true);
        assert!(staged["branch"].get("close_reason").is_none());
        let legacy_branch: BranchPushData = serde_json::from_value(serde_json::json!({
            "name": "main",
            "description": null,
            "parent_branch": null,
            "status": "open",
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(legacy_branch.close_reason, None);
        let mut missing_boundary = staged.clone();
        missing_boundary
            .as_object_mut()
            .unwrap()
            .remove("expected_branch_head");
        assert!(serde_json::from_value::<StagedPushRequest>(missing_boundary).is_err());

        let legacy_staged: StagedPushRequest = serde_json::from_value(serde_json::json!({
            "stage_id": "0123456789abcdef0123456789abcdef",
            "expected_branch_head": null,
            "branch": {
                "name": "main",
                "description": null,
                "parent_branch": null,
                "status": "open",
                "created_at": "2026-01-01T00:00:00Z"
            },
            "finalize": false,
            "target_head": null,
            "commits": [],
            "blobs": [],
            "trees": []
        }))
        .unwrap();
        assert!(
            !legacy_staged.force,
            "omitted force must remain fail-closed"
        );

        let ordinary_info = serde_json::to_value(CommitInfoRequest {
            hashes: vec!["cd".repeat(32)],
            metadata_only: false,
        })
        .unwrap();
        assert!(ordinary_info.get("metadata_only").is_none());
        let metadata_info = serde_json::to_value(CommitInfoRequest {
            hashes: vec!["cd".repeat(32)],
            metadata_only: true,
        })
        .unwrap();
        assert_eq!(metadata_info["metadata_only"], true);

        let abort = serde_json::to_value(StagedAbortRequest {
            branch_name: "main".to_string(),
            expected_branch_head: None,
        })
        .unwrap();
        assert_eq!(abort["branch_name"], "main");
        assert!(abort.get("expected_branch_head").is_some());
        let response: StagedAbortResponse = serde_json::from_value(serde_json::json!({
            "aborted": true,
            "state": "aborted"
        }))
        .unwrap();
        assert!(response.aborted);
        assert_eq!(response.state, "aborted");
    }

    #[test]
    fn pull_missing_content_is_append_only_and_operator_adjudicated() {
        let legacy: PullResponse = serde_json::from_value(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        }))
        .unwrap();
        assert!(legacy.missing_content.is_empty());

        let response: PullResponse = serde_json::from_value(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": [],
            "missing_content": [{
                "kind": "blob",
                "hash": "ab".repeat(32),
                "reason_code": "operator_adjudicated_loss"
            }]
        }))
        .unwrap();
        assert_eq!(response.missing_content.len(), 1);
        assert_eq!(response.missing_content[0].kind, "blob");
        assert_eq!(
            response.missing_content[0].reason_code,
            "operator_adjudicated_loss"
        );
    }

    #[test]
    fn pull_known_loss_protocol_is_explicit_and_omitted_for_legacy_requests() {
        let mut query = PullQuery {
            since: None,
            branch_name: None,
            force: false,
            since_rename_id: 0,
            depth: None,
            known_loss_protocol: None,
        };
        let legacy = serde_json::to_value(&query).unwrap();
        assert!(legacy.get("known_loss_protocol").is_none());

        query.known_loss_protocol = Some("report_v1".to_string());
        let opted_in = serde_json::to_value(&query).unwrap();
        assert_eq!(opted_in["known_loss_protocol"], "report_v1");
    }

    #[test]
    fn chunk_batch_protocol_is_append_only_and_explicit() {
        let legacy_check: ChunkCheckRequest = serde_json::from_value(serde_json::json!({
            "hashes": [],
            "sizes": []
        }))
        .unwrap();
        assert_eq!(legacy_check.chunk_batch_protocol, None);
        let legacy_download: ChunkDownloadRequest =
            serde_json::from_value(serde_json::json!({ "hashes": [] })).unwrap();
        assert_eq!(legacy_download.chunk_batch_protocol, None);

        let check = serde_json::to_value(ChunkCheckRequest {
            hashes: vec!["ab".repeat(32)],
            sizes: Some(vec![1]),
            chunk_batch_protocol: Some("bounded_v1".to_string()),
        })
        .unwrap();
        assert_eq!(check["chunk_batch_protocol"], "bounded_v1");
        let download = serde_json::to_value(ChunkDownloadRequest {
            hashes: vec!["ab".repeat(32)],
            chunk_batch_protocol: Some("bounded_v1".to_string()),
        })
        .unwrap();
        assert_eq!(download["chunk_batch_protocol"], "bounded_v1");
    }

    fn sample_tree() -> Tree {
        Tree::new(vec![
            TreeEntry {
                name: "a.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: Hash::from_hex(&"11".repeat(32)).unwrap(),
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "b.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: Hash::from_hex(&"22".repeat(32)).unwrap(),
                mode: FileMode::Regular,
            },
        ])
        .unwrap()
    }

    #[test]
    fn tree_data_to_core_round_trips_a_valid_tree() {
        let tree = sample_tree();
        let wire = tree_to_wire(&tree);
        let back = tree_data_to_core(&wire).expect("valid tree must convert");
        assert_eq!(back.hash, tree.hash);
        assert_eq!(back.entries.len(), 2);
    }

    #[test]
    fn tree_data_to_core_rejects_tampered_entry_under_claimed_hash() {
        // A peer keeps the legitimate tree hash but swaps an entry's blob hash
        // to point at arbitrary content. This must be rejected, not stored.
        let tree = sample_tree();
        let mut wire = tree_to_wire(&tree);
        wire.entries[0].hash = "99".repeat(32); // different blob, same tree hash claimed
        let err = tree_data_to_core(&wire)
            .expect_err("a tree whose content doesn't match its hash must be rejected");
        assert!(
            err.contains("hash mismatch"),
            "expected a hash-mismatch error, got: {err}"
        );
    }

    #[test]
    fn tree_data_to_core_canonicalizes_entry_order() {
        // Even if a peer sends entries out of canonical order, the verified
        // tree comes back sorted and still matches the claimed hash.
        let tree = sample_tree();
        let mut wire = tree_to_wire(&tree);
        wire.entries.reverse();
        let back = tree_data_to_core(&wire).expect("order must not affect validity");
        assert_eq!(back.hash, tree.hash);
        assert_eq!(back.entries[0].name, "a.txt");
    }

    #[test]
    fn tree_data_to_core_rejects_invalid_mode() {
        let tree = sample_tree();
        let mut wire = tree_to_wire(&tree);
        wire.entries[0].mode = "mystery".to_string();
        let err = tree_data_to_core(&wire).expect_err("invalid mode must be rejected");
        assert!(
            err.contains("invalid tree entry mode"),
            "unexpected error: {err}"
        );
    }

    fn commit_to_wire(commit: &Commit) -> CommitData {
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
                    old_mode: f.old_mode.map(|mode| file_mode_to_wire(mode).to_string()),
                    new_mode: f.new_mode.map(|mode| file_mode_to_wire(mode).to_string()),
                })
                .collect(),
        }
    }

    fn sample_commit() -> CommitData {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-06-24T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            None,
            vec![FileChange {
                path: "src/lib.rs".to_string(),
                change_type: ChangeType::Modified,
                old_blob_hash: Some(Hash::from_hex(&"11".repeat(32)).unwrap()),
                new_blob_hash: Some(Hash::from_hex(&"22".repeat(32)).unwrap()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            }],
            timestamp,
        )
        .unwrap();
        commit_to_wire(&commit)
    }

    #[test]
    fn commit_data_to_core_round_trips_valid_commit() {
        let wire = sample_commit();
        let commit = commit_data_to_core(&wire).expect("valid commit must convert");
        assert_eq!(commit.hash.to_string(), wire.hash);
        assert_eq!(commit.branch_name, "main");
        assert_eq!(commit.files.len(), 1);
    }

    /// Pin the actual commit hash produced for `sample_commit`'s fixed inputs.
    ///
    /// `commit_data_to_core_round_trips_valid_commit` only proves the wire hash
    /// and the recomputed hash agree — but both are produced by the same code,
    /// so a change to the commit hash format moves them together and that test
    /// stays green. This freezes the concrete value, so any drift in the v1
    /// commit preimage is caught at the exact boundary where it would otherwise
    /// surface in production: a server commit failing `commit hash mismatch`
    /// against a client that hashes differently.
    #[test]
    fn commit_data_to_core_hash_is_stable() {
        let wire = sample_commit();
        assert_eq!(
            wire.hash,
            "93dd864faf6f23324fa2010c7f6d51b5aa456686586a2b65c132575661866b0a"
        );
    }

    #[test]
    fn commit_data_to_core_rejects_hash_mismatch() {
        let mut wire = sample_commit();
        wire.hash = "99".repeat(32);
        let err = commit_data_to_core(&wire).expect_err("tampered commit hash must be rejected");
        assert!(
            err.contains("commit hash mismatch"),
            "expected a commit hash mismatch, got: {err}"
        );
    }

    /// A commit hashed with a nanosecond timestamp whose stored/wire timestamp
    /// was truncated to microseconds (Postgres `TIMESTAMPTZ`) must still
    /// verify — with the lost digits provably recovered, not trusted. Every
    /// squash-merge the server created before `Commit::new` truncated is in
    /// this state.
    #[test]
    fn commit_data_to_core_recovers_micro_truncated_timestamp() {
        let nano_ts = chrono::DateTime::parse_from_rfc3339("2026-07-04T08:25:15.778307228Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            Tree::empty_hash(),
            "tester".to_string(),
            Some("squash message".to_string()),
            Vec::new(),
            nano_ts,
        )
        .unwrap();

        let mut wire = commit_to_wire(&commit);
        // Simulate the microsecond truncation the server's store applied.
        wire.timestamp = "2026-07-04T08:25:15.778307+00:00".to_string();

        let back = commit_data_to_core(&wire).expect("truncated timestamp must be recovered");
        assert_eq!(back.hash, commit.hash);
        // The recovered timestamp is the original nanosecond one, so the
        // returned commit is exactly self-verifying when re-stored.
        assert_eq!(back.timestamp, nano_ts);

        // Truncation tolerance must not accept genuinely wrong fields.
        let mut tampered = commit_to_wire(&commit);
        tampered.timestamp = "2026-07-04T08:25:15.778307+00:00".to_string();
        tampered.author = "someone-else".to_string();
        let err = commit_data_to_core(&tampered).expect_err("tampered fields must be rejected");
        assert!(
            err.contains("commit hash mismatch"),
            "expected a commit hash mismatch, got: {err}"
        );
    }

    #[test]
    fn commit_data_to_core_rejects_invalid_hash_fields_and_change_types() {
        let mut wire = sample_commit();
        wire.parent_hash = Some("not-a-hash".to_string());
        let err = commit_data_to_core(&wire).expect_err("invalid parent hash must be rejected");
        assert!(
            err.contains("invalid parent hash"),
            "unexpected error: {err}"
        );

        let mut wire = sample_commit();
        wire.files[0].change_type = "rewritten".to_string();
        let err = commit_data_to_core(&wire).expect_err("invalid change type must be rejected");
        assert!(
            err.contains("invalid file change #0 type"),
            "unexpected error: {err}"
        );
    }
}
