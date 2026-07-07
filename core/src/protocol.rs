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

use crate::{ChangeType, Commit, FileChange, FileMode, Hash, Tree, TreeEntry, TreeEntryKind};

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
    /// Batch-download endpoint (CDN Worker) carrying a capability token, set
    /// only when the chunk CDN is configured. Clients may POST a
    /// `{ "hashes": [...] }` body here to fetch many small chunks in one
    /// length-framed response instead of one GET per chunk. `None` → fall back
    /// to per-chunk `download_url`/`content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_url: Option<String>,
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

    Ok(FileChange {
        path: f.path.clone(),
        change_type,
        old_blob_hash: wire_optional_hash("old blob", f.old_blob_hash.as_ref())?,
        new_blob_hash: wire_optional_hash("new blob", f.new_blob_hash.as_ref())?,
        old_path: f.old_path.clone(),
        old_mode: None,
        new_mode: None,
    })
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
