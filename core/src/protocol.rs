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

use serde::{Deserialize, Serialize};

use crate::{FileMode, Hash, Tree, TreeEntry, TreeEntryKind};

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
    #[serde(default)]
    pub projects: Vec<ProjectResponse>,
}

/// Project info included in the repository picker response.
#[derive(Serialize, Deserialize)]
pub struct ProjectResponse {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub path_prefix: String,
    pub visibility: String,
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
#[derive(Serialize, Deserialize)]
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
#[derive(Serialize, Deserialize)]
pub struct FileChangeData {
    pub path: String,
    pub change_type: String,
    pub old_blob_hash: Option<String>,
    pub new_blob_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// Chunk reference for chunked blob transfer.
#[derive(Serialize, Deserialize, Clone)]
pub struct ChunkRefData {
    pub hash: String,
    pub offset: u64,
    pub size: u32,
}

/// Blob data for transfer.
#[derive(Serialize, Deserialize)]
pub struct BlobData {
    pub hash: String,
    pub content: Vec<u8>,
    pub size: u64,
    /// Chunk metadata for large/chunked blobs (empty for small blobs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ChunkRefData>,
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

/// Branch push data (branch-like container metadata).
#[derive(Serialize, Deserialize)]
pub struct BranchPushData {
    pub name: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: String,
    pub created_at: String,
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
    /// Slug of a team to filter the response by. When set, the server
    /// resolves the team's projects' `path_prefix` values for this repo
    /// and only emits blob content / chunk refs for paths under any of
    /// them. Tree objects are always emitted in full so the client can
    /// preserve out-of-scope manifest entries during commit-under-scope.
    /// Older clients omit this field and continue to receive the full
    /// graph. Mutually exclusive with `project`. (Org-less servers ignore it.)
    pub team: Option<String>,
    /// Slug of a project to filter the response by. Behaves like `team`
    /// but binds to one project's `path_prefix`.
    pub project: Option<String>,
    /// Raw path prefix filter (e.g. `src/api`). Leading slash is optional.
    /// Used by `oak clone owner/repo/path` to scope a clone to a subtree
    /// without requiring a named project. Mutually exclusive with `team`
    /// and `project`.
    pub prefix: Option<String>,
    /// Shallow-clone depth. When set (and no `branch_name` is given — i.e. the
    /// clone case), the server returns only the most recent `depth` commits on
    /// the default branch instead of the full reachable history. `oak clone`
    /// sends `depth=1` by default; `oak clone --full` omits it to fetch
    /// everything. Ignored on branch-scoped pulls. A value of `0` is treated
    /// as "no limit" (same as unset).
    pub depth: Option<u32>,
}

/// Branch pull data (branch-like container metadata).
#[derive(Serialize, Deserialize)]
pub struct BranchPullData {
    pub name: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: String,
    pub created_at: String,
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
    /// Path prefixes the response was filtered to (from `?team=` or
    /// `?project=`). Empty for unscoped pulls; clients use this list to
    /// materialize the working tree and persist the local scope so future
    /// commits/pulls reapply the same filter. Older servers omit this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_prefixes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Blob existence / metadata
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct BlobCheckRequest {
    pub hashes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct BlobCheckResponse {
    /// Subset of the request's `hashes` the server does NOT have. The
    /// client should send only these in the push payload.
    pub missing: Vec<String>,
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
}

#[derive(Serialize, Deserialize)]
pub struct CommitInfoRequest {
    pub hashes: Vec<String>,
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

/// Convert a wire [`TreeData`] to a core [`Tree`]. Validates `kind` and `mode`.
pub fn tree_data_to_core(td: &TreeData) -> Result<Tree, String> {
    let mut entries = Vec::with_capacity(td.entries.len());
    for e in &td.entries {
        let kind = match e.kind.as_str() {
            "tree" => TreeEntryKind::Tree,
            "blob" => TreeEntryKind::Blob,
            other => return Err(format!("invalid tree entry kind: {other}")),
        };
        let mode = match e.mode.as_str() {
            "executable" => FileMode::Executable,
            "symlink" => FileMode::Symlink,
            // "tree" or "regular" or anything else falls back to Regular,
            // which is also the canonical mode for tree entries.
            _ => FileMode::Regular,
        };
        entries.push(TreeEntry {
            name: e.name.clone(),
            kind,
            hash: Hash(e.hash.clone()),
            mode,
        });
    }
    Ok(Tree {
        hash: Hash(td.hash.clone()),
        entries,
    })
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
