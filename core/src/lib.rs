pub mod capabilities;
pub mod chunking;
pub mod diff;
pub mod engine;
pub mod error;
pub mod features;
pub mod hash;
pub mod hash_format;
pub mod ignore;
pub mod merge;
/// Wire protocol DTOs for the push/pull/clone sync protocol (shared by the
/// CLI client, the hosted server, and `oak serve`). Not gated behind
/// `local-repo` — server-side consumers need it with `default-features = false`.
pub mod protocol;
pub mod tree;

// Client-side local repository, gated behind the default `local-repo` feature.
#[cfg(feature = "local-repo")]
pub mod git;
#[cfg(feature = "local-repo")]
pub mod sqlite;
#[cfg(feature = "local-repo")]
pub mod traits;

pub use capabilities::Capabilities;
pub use chunking::{chunk_content, reassemble_chunks, ChunkInfo};
#[cfg(feature = "local-repo")]
pub use chunking::{chunk_decode, chunk_encode};
pub use diff::{DiffLine, FileDiff, FileStatus, Hunk};
pub use engine::{detect_engine, GameEngine};
pub use error::{OakError, Result};
pub use hash::{hash_bytes, hash_string, Hash, HashKind};
pub use hash_format::HashFormat;
pub use ignore::{is_system_metadata_path, IgnorePatterns};
pub use merge::{
    three_way_merge_manifests, three_way_merge_text, MergeConflict, MergeOutcome, TextMerge,
};
pub use tree::{
    build_tree, collect_tree_objects, diff_trees, flatten_tree, normalize_path, resolve_path,
    subtree_at_path, validate_entry_name, validate_tree_path, BuiltTree, Tree, TreeEntry,
    TreeEntryKind,
};

// Client-side storage: the synchronous `Repository` trait, its local SQLite
// backend, and a read-only git backend. Merged in from the former
// `oak-storage` crate so this one crate carries both the data model and a
// local repository implementation. Gated behind the default `local-repo`
// feature.
#[cfg(feature = "local-repo")]
pub use git::GitRepository;
#[cfg(feature = "local-repo")]
pub use sqlite::{BulkImporter, CompactStats, SqliteRepository};
#[cfg(feature = "local-repo")]
pub use traits::{Repository, StatCacheEntry};

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

/// Files larger than this threshold are transferred separately during
/// push/pull for better performance and memory efficiency.
pub const LARGE_FILE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB
use serde::{Deserialize, Serialize};

/// A content-addressed blob (file content)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub hash: Hash,
    pub content: Vec<u8>,
    pub size: u64,
}

impl Blob {
    /// Create a new blob from content
    pub fn new(content: Vec<u8>) -> Self {
        let hash = hash_bytes(&content);
        let size = content.len() as u64;
        Blob {
            hash,
            content,
            size,
        }
    }

    /// Create a blob from a string
    pub fn from_string(s: &str) -> Self {
        Self::new(s.as_bytes().to_vec())
    }

    /// Create a blob from a string (alias for from_string)
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self::from_string(s)
    }
}

/// File mode/permissions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FileMode {
    /// Regular file (644)
    #[default]
    Regular,
    /// Executable file (755)
    Executable,
    /// Symbolic link
    Symlink,
}

/// An entry in a manifest (a file at a specific path)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub blob_hash: Hash,
    pub mode: FileMode,
}

/// A manifest represents the complete state of the repository at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub hash: Hash,
    pub entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// Create a manifest from entries, computing the hash as the **root tree
    /// hash** of the equivalent nested tree. This makes `manifest.hash`
    /// interchangeable with the tree storage produced by [`tree::build_tree`]:
    /// callers that store via `put_tree(entries)` and callers that store via
    /// `Manifest::new(entries).hash` end up with the same content-addressed key.
    pub fn new(mut entries: Vec<ManifestEntry>) -> Self {
        // Sort entries by path for deterministic ordering of the flat view.
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        // Unchecked: the hash is only a diff identity here and no tree object
        // is stored. Storage paths (`put_tree`/`put_manifest`/`atomic_commit`)
        // go through the validating `tree::build_tree`.
        let hash = tree::build_tree_unchecked(&entries).root_hash;
        Manifest { hash, entries }
    }

    /// Create an empty manifest
    pub fn empty() -> Self {
        Manifest {
            hash: tree::Tree::empty_hash(),
            entries: Vec::new(),
        }
    }

    /// Get an entry by path
    pub fn get(&self, path: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// Compare with another manifest and return changes (with default rename detection)
    pub fn diff(&self, other: &Manifest) -> Vec<FileChange> {
        self.diff_with_renames(other, &RenameConfig::default())
    }

    /// Compare with another manifest and return changes, detecting renames
    /// using the provided configuration.
    ///
    /// Only detects renames where the old and new blob hashes are identical
    /// (pure renames with no edits). To catch rename-with-edit, post-process
    /// the result with [`detect_renames_by_content`] — that pass needs blob
    /// content, which a pure manifest-level diff doesn't have.
    pub fn diff_with_renames(&self, other: &Manifest, _config: &RenameConfig) -> Vec<FileChange> {
        let mut changes = Vec::new();
        let mut deleted: Vec<(&str, &ManifestEntry)> = Vec::new();
        let mut added: Vec<(&str, &ManifestEntry)> = Vec::new();

        // Build lookup maps
        let self_map: HashMap<&str, &ManifestEntry> =
            self.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        let other_map: HashMap<&str, &ManifestEntry> =
            other.entries.iter().map(|e| (e.path.as_str(), e)).collect();

        // Find removed and modified files
        for (path, entry) in &self_map {
            match other_map.get(path) {
                None => {
                    deleted.push((path, entry));
                }
                Some(other_entry) => {
                    if entry.blob_hash != other_entry.blob_hash || entry.mode != other_entry.mode {
                        changes.push(FileChange {
                            path: path.to_string(),
                            change_type: ChangeType::Modified,
                            old_blob_hash: Some(entry.blob_hash.clone()),
                            new_blob_hash: Some(other_entry.blob_hash.clone()),
                            old_path: None,
                            old_mode: Some(entry.mode),
                            new_mode: Some(other_entry.mode),
                        });
                    }
                }
            }
        }

        // Find added files
        for (path, entry) in &other_map {
            if !self_map.contains_key(path) {
                added.push((path, entry));
            }
        }

        // Rename detection: exact blob hash matches only. Rename-with-edit
        // is recovered later by detect_renames_by_content, which has access
        // to blob content.
        // Preserve the old first-match behavior without scanning every add
        // for every delete.
        let mut added_by_hash: HashMap<&Hash, VecDeque<usize>> = HashMap::new();
        for (idx, (_, add_entry)) in added.iter().enumerate() {
            added_by_hash
                .entry(&add_entry.blob_hash)
                .or_default()
                .push_back(idx);
        }

        let mut matched_added = vec![false; added.len()];
        for (del_path, del_entry) in &deleted {
            match added_by_hash
                .get_mut(&del_entry.blob_hash)
                .and_then(VecDeque::pop_front)
            {
                Some(add_idx) => {
                    let (add_path, add_entry) = added[add_idx];
                    changes.push(FileChange {
                        path: add_path.to_string(),
                        change_type: ChangeType::Renamed,
                        old_blob_hash: Some(del_entry.blob_hash.clone()),
                        new_blob_hash: Some(add_entry.blob_hash.clone()),
                        old_path: Some(del_path.to_string()),
                        old_mode: Some(del_entry.mode),
                        new_mode: Some(add_entry.mode),
                    });
                    matched_added[add_idx] = true;
                }
                None => {
                    changes.push(FileChange {
                        path: del_path.to_string(),
                        change_type: ChangeType::Deleted,
                        old_blob_hash: Some(del_entry.blob_hash.clone()),
                        new_blob_hash: None,
                        old_path: None,
                        old_mode: Some(del_entry.mode),
                        new_mode: None,
                    });
                }
            }
        }

        for (idx, (add_path, add_entry)) in added.iter().enumerate() {
            if !matched_added[idx] {
                changes.push(FileChange {
                    path: add_path.to_string(),
                    change_type: ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(add_entry.blob_hash.clone()),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(add_entry.mode),
                });
            }
        }

        // Sort by path for consistent ordering
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }
}

/// Type of change to a file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeType::Added => write!(f, "A"),
            ChangeType::Modified => write!(f, "M"),
            ChangeType::Deleted => write!(f, "D"),
            ChangeType::Renamed => write!(f, "R"),
        }
    }
}

/// A file change in a commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub change_type: ChangeType,
    pub old_blob_hash: Option<Hash>,
    pub new_blob_hash: Option<Hash>,
    /// For renames, the original path before renaming
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// File mode before the change. `None` for additions, or when the
    /// producer didn't carry mode through (e.g. a FileChange built from a
    /// wire payload that omits mode). Populated by [`Manifest::diff_with_renames`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<FileMode>,
    /// File mode after the change. `None` for deletions, or when the
    /// producer didn't carry mode through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<FileMode>,
}

/// Configuration for rename detection heuristics
#[derive(Debug, Clone)]
pub struct RenameConfig {
    /// Minimum similarity ratio (0.0-1.0) for [`detect_renames_by_content`]
    /// to pair a delete+add as a rename. Exact blob-hash matches are always
    /// detected by [`Manifest::diff_with_renames`] regardless of this value.
    pub similarity_threshold: f64,

    /// Per-file size cap (bytes) for content-similarity comparison. Files
    /// larger than this on either side of a pair are skipped — rename
    /// detection only fires when both contents fit in memory. Default: 1 MiB.
    pub max_file_bytes: usize,

    /// Maximum number of (delete + add) candidates to consider in
    /// [`detect_renames_by_content`]. Past this, the pass bails out without
    /// running similarity scoring — this prevents the O(D*A) pair scan from
    /// pathologically slowing huge refactor commits. Default: 1000.
    pub max_candidates: usize,
}

impl Default for RenameConfig {
    fn default() -> Self {
        RenameConfig {
            similarity_threshold: 0.5,
            max_file_bytes: 1 << 20,
            max_candidates: 1000,
        }
    }
}

/// Compute a content-similarity score between two byte slices (0.0-1.0).
///
/// Uses line-based Jaccard: returns `|A ∩ B| / |A ∪ B|` over the set of
/// distinct lines on each side. Binary inputs (anything with a NUL byte
/// in the first 8 KiB) and empty inputs score 0.0 — those fall back to
/// plain add/delete unless their blob hashes were already identical.
pub fn content_similarity(old: &[u8], new: &[u8]) -> f64 {
    if old.is_empty() || new.is_empty() {
        return 0.0;
    }
    const BINARY_SNIFF: usize = 8192;
    if old.iter().take(BINARY_SNIFF).any(|b| *b == 0)
        || new.iter().take(BINARY_SNIFF).any(|b| *b == 0)
    {
        return 0.0;
    }

    fn line_hashes(content: &[u8]) -> HashSet<u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash as _, Hasher};
        content
            .split(|b| *b == b'\n')
            .map(|line| {
                let mut h = DefaultHasher::new();
                line.hash(&mut h);
                h.finish()
            })
            .collect()
    }

    let a = line_hashes(old);
    let b = line_hashes(new);
    let inter = a.intersection(&b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Promote Add+Delete pairs in `changes` to Renamed when their contents are
/// similar enough. `get_content` is called with each candidate's blob hash;
/// returning `Ok(None)` (missing blob) silently skips that candidate.
///
/// Greedy best-similarity-first matching: the highest-scoring pair is locked
/// in first, then the next non-conflicting pair, and so on. Pairs whose
/// similarity is below `config.similarity_threshold` are never matched.
///
/// Bails out without scanning when the candidate count exceeds
/// `config.max_candidates` — content-similarity rename detection is O(D*A)
/// in the candidate counts, and that bound stops a 50 000-file move from
/// stalling `oak commit`.
pub fn detect_renames_by_content<F>(
    changes: &mut Vec<FileChange>,
    mut get_content: F,
    config: &RenameConfig,
) -> Result<()>
where
    F: FnMut(&Hash) -> Result<Option<Vec<u8>>>,
{
    let mut deleted_idxs: Vec<usize> = Vec::new();
    let mut added_idxs: Vec<usize> = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        match c.change_type {
            ChangeType::Deleted => deleted_idxs.push(i),
            ChangeType::Added => added_idxs.push(i),
            _ => {}
        }
    }
    if deleted_idxs.is_empty() || added_idxs.is_empty() {
        return Ok(());
    }
    if deleted_idxs.len() + added_idxs.len() > config.max_candidates {
        return Ok(());
    }

    let max_bytes = config.max_file_bytes;

    fn fetch_for<F>(
        idx: usize,
        hash: Option<&Hash>,
        max_bytes: usize,
        get_content: &mut F,
    ) -> Result<Option<(usize, Hash, Vec<u8>)>>
    where
        F: FnMut(&Hash) -> Result<Option<Vec<u8>>>,
    {
        let Some(h) = hash else { return Ok(None) };
        let Some(bytes) = get_content(h)? else {
            return Ok(None);
        };
        if bytes.len() > max_bytes {
            return Ok(None);
        }
        Ok(Some((idx, h.clone(), bytes)))
    }

    let mut del_contents: Vec<(usize, Hash, Vec<u8>)> = Vec::new();
    for &di in &deleted_idxs {
        if let Some(entry) = fetch_for(
            di,
            changes[di].old_blob_hash.as_ref(),
            max_bytes,
            &mut get_content,
        )? {
            del_contents.push(entry);
        }
    }
    let mut add_contents: Vec<(usize, Hash, Vec<u8>)> = Vec::new();
    for &ai in &added_idxs {
        if let Some(entry) = fetch_for(
            ai,
            changes[ai].new_blob_hash.as_ref(),
            max_bytes,
            &mut get_content,
        )? {
            add_contents.push(entry);
        }
    }

    let mut scores: Vec<(usize, usize, f64)> = Vec::new();
    for (di, _, del_bytes) in &del_contents {
        for (ai, _, add_bytes) in &add_contents {
            let s = content_similarity(del_bytes, add_bytes);
            if s >= config.similarity_threshold {
                scores.push((*di, *ai, s));
            }
        }
    }
    scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut used_del: HashSet<usize> = HashSet::new();
    let mut used_add: HashSet<usize> = HashSet::new();
    let mut renames: Vec<FileChange> = Vec::new();
    for (di, ai, _) in &scores {
        if used_del.contains(di) || used_add.contains(ai) {
            continue;
        }
        let del = &changes[*di];
        let add = &changes[*ai];
        renames.push(FileChange {
            path: add.path.clone(),
            change_type: ChangeType::Renamed,
            old_blob_hash: del.old_blob_hash.clone(),
            new_blob_hash: add.new_blob_hash.clone(),
            old_path: Some(del.path.clone()),
            old_mode: del.old_mode,
            new_mode: add.new_mode,
        });
        used_del.insert(*di);
        used_add.insert(*ai);
    }

    if renames.is_empty() {
        return Ok(());
    }

    let to_drop: HashSet<usize> = used_del.iter().chain(used_add.iter()).cloned().collect();
    let kept: Vec<FileChange> = std::mem::take(changes)
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| if to_drop.contains(&i) { None } else { Some(c) })
        .collect();
    *changes = kept;
    changes.extend(renames);
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// A commit within a branch.
///
/// # Commit messages and the new model
///
/// As of the "branch descriptions are the source of truth" refactor, commit
/// messages no longer exist for local commits or feature-branch commits on
/// the server. The `message` field is `Some(_)` only for commits on `main`
/// that were produced by a squash-merge — and in that case the message is
/// the merged branch's description. Everything else is `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub hash: Hash,
    pub branch_name: String,
    pub parent_hash: Option<Hash>,
    pub merge_parent_hash: Option<Hash>,
    pub manifest_hash: Hash,
    pub author: String,
    /// `Some(_)` only on main-branch squash-merge commits (where the value is
    /// the merged branch's description). `None` for every local commit and
    /// every feature-branch commit on the server.
    #[serde(default)]
    pub message: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub files: Vec<FileChange>,
}

impl Commit {
    /// Create a new commit, computing the hash
    pub fn new(
        branch_name: String,
        parent_hash: Option<Hash>,
        merge_parent_hash: Option<Hash>,
        manifest_hash: Hash,
        author: String,
        message: Option<String>,
        files: Vec<FileChange>,
    ) -> Result<Self> {
        Self::with_timestamp(
            branch_name,
            parent_hash,
            merge_parent_hash,
            manifest_hash,
            author,
            message,
            files,
            Utc::now(),
        )
    }

    /// Create a commit at a specific timestamp (instead of `Utc::now()`).
    /// Used when replaying commits from another VCS — `oak clone <git-url>`
    /// imports git history and needs the original commit dates preserved.
    ///
    /// The hash preimage joins its fields with `\n` (see
    /// [`hash_format::hash_commit`]), so branch name and author must be
    /// `\n`/`\t`/`\0`-free or two distinct commits could hash identically.
    /// Both are Oak-generated or config values, so rejecting them here never
    /// refuses previously legal data. The message — the one field that may
    /// legitimately contain newlines — stays unambiguous because every
    /// neighboring field is now control-character-free.
    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamp(
        branch_name: String,
        parent_hash: Option<Hash>,
        merge_parent_hash: Option<Hash>,
        manifest_hash: Hash,
        author: String,
        message: Option<String>,
        files: Vec<FileChange>,
        timestamp: DateTime<Utc>,
    ) -> Result<Self> {
        validate_commit_field("branch name", &branch_name)?;
        validate_commit_field("author", &author)?;

        // `None` message hashes like `Some("")` in v1 — that's part of the
        // on-the-wire hash derivation. Fine: both represent "no human-written
        // message," and we don't intentionally produce `Some("")` anywhere.
        let hash = hash_format::hash_commit(
            hash_format::HashFormat::V1,
            &hash_format::CommitFields {
                branch_name: &branch_name,
                parent_hash: parent_hash.as_ref(),
                merge_parent_hash: merge_parent_hash.as_ref(),
                manifest_hash: &manifest_hash,
                author: &author,
                message: message.as_deref(),
                timestamp: &timestamp,
                files: &files,
            },
        );

        Ok(Commit {
            hash,
            branch_name,
            parent_hash,
            merge_parent_hash,
            manifest_hash,
            author,
            message,
            timestamp,
            files,
        })
    }
}

/// Reject `\n`/`\t`/`\0` in a commit-hash preimage field. The preimage is
/// `\n`-joined, so a control character here would let two distinct commits
/// share one hash. See [`Commit::with_timestamp`].
fn validate_commit_field(field: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\t', '\0']) {
        return Err(OakError::InvalidArgument(format!(
            "commit {field} {value:?} contains a control character (newline, tab, or NUL)"
        )));
    }
    Ok(())
}

/// Status of a branch
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchStatus {
    Open,
    Closed,
}

impl BranchStatus {
    /// Canonical lowercase name used in DB rows and JSON wire formats.
    pub fn as_str(&self) -> &'static str {
        match self {
            BranchStatus::Open => "open",
            BranchStatus::Closed => "closed",
        }
    }

    /// Parse a status string from a DB row or wire payload. Unknown values
    /// fall back to `Open` (matching the pre-existing behavior at every
    /// storage backend that previously did `_ => Open`).
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "closed" => BranchStatus::Closed,
            _ => BranchStatus::Open,
        }
    }
}

impl std::fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A branch (container for commits)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: BranchStatus,
    pub created_at: DateTime<Utc>,
}

impl Branch {
    /// Create a new branch
    pub fn new(name: String, description: Option<String>, parent_branch: Option<String>) -> Self {
        Branch {
            name,
            description,
            parent_branch,
            status: BranchStatus::Open,
            created_at: Utc::now(),
        }
    }
}

/// The repository trunk. Oak's branching model is intentionally *flat*:
/// branch-per-task off the trunk, and merge lands the branch back onto the
/// trunk. A branch is never stacked on another feature branch — that creates
/// confusing parent chains (a branch that can't merge cleanly until its parent
/// merges first) and is exactly the relationship we forbid. The merge,
/// "behind main", and default-head logic all assume this single trunk.
pub const DEFAULT_BRANCH: &str = "main";

/// Whether `parent` is a legal parent for a branch under the flat model. The
/// only allowed parent is the trunk ([`DEFAULT_BRANCH`]); `None` is allowed for
/// the trunk itself (which has no parent). Every other value — i.e. parenting a
/// branch onto another feature branch — is rejected.
pub fn is_allowed_parent(parent: Option<&str>) -> bool {
    matches!(parent, None | Some(DEFAULT_BRANCH))
}

/// A single rename event in the server's append-only rename log.
/// Returned in pull responses so other clients can replay the rename
/// against their local repo before applying new commits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRename {
    pub id: i64,
    pub old_name: String,
    pub new_name: String,
    pub renamed_at: DateTime<Utc>,
}

/// Repository metadata key-value pairs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataKey {
    Head,
    RemoteUrl,
    RepoName,
    RepoOwner,
    CurrentBranch,
    ApiKey,
    LastRenameId,
    MainLastCheckedAt,
    /// Repo-level content-addressing format (see [`crate::hash_format`]).
    /// Absent means v1; nothing writes this key yet — v2 is prepared but
    /// cannot be enabled until an explicit migration ships.
    HashFormat,
}

impl MetadataKey {
    pub fn as_str(&self) -> &'static str {
        match self {
            MetadataKey::Head => "head",
            MetadataKey::RemoteUrl => "remote_url",
            MetadataKey::RepoName => "repo_name",
            MetadataKey::RepoOwner => "repo_owner",
            MetadataKey::CurrentBranch => "current_branch",
            MetadataKey::ApiKey => "api_key",
            MetadataKey::LastRenameId => "last_rename_id",
            MetadataKey::MainLastCheckedAt => "main_last_checked_at",
            MetadataKey::HashFormat => "hash_format",
        }
    }
}

impl std::fmt::Display for MetadataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A tag pointing to a specific commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub commit_hash: Hash,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_parent_is_trunk_only() {
        // The trunk itself (no parent) is allowed.
        assert!(is_allowed_parent(None));
        // Parenting onto the trunk is allowed.
        assert!(is_allowed_parent(Some(DEFAULT_BRANCH)));
        assert!(is_allowed_parent(Some("main")));
        // Parenting onto any other branch is rejected — no stacked branches.
        assert!(!is_allowed_parent(Some("feature-x")));
        assert!(!is_allowed_parent(Some("some-task--ab12cd34")));
        assert!(!is_allowed_parent(Some("")));
    }

    #[test]
    fn test_blob_creation() {
        let content = b"hello world".to_vec();
        let blob = Blob::new(content.clone());
        assert_eq!(blob.content, content);
        assert_eq!(blob.size, 11);
    }

    #[test]
    fn test_blob_hash_determinism() {
        let blob1 = Blob::from_str("test content");
        let blob2 = Blob::from_str("test content");
        assert_eq!(blob1.hash, blob2.hash);
    }

    #[test]
    fn test_manifest_hash_determinism() {
        let entries = vec![
            ManifestEntry {
                path: "file1.txt".to_string(),
                blob_hash: hash_string("content1"),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "file2.txt".to_string(),
                blob_hash: hash_string("content2"),
                mode: FileMode::Regular,
            },
        ];

        let manifest1 = Manifest::new(entries.clone());
        let manifest2 = Manifest::new(entries);
        assert_eq!(manifest1.hash, manifest2.hash);
    }

    #[test]
    fn test_manifest_diff_added() {
        let old = Manifest::empty();
        let new = Manifest::new(vec![ManifestEntry {
            path: "new.txt".to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }]);

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Added);
    }

    #[test]
    fn test_manifest_diff_deleted() {
        let old = Manifest::new(vec![ManifestEntry {
            path: "old.txt".to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }]);
        let new = Manifest::empty();

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Deleted);
    }

    #[test]
    fn test_manifest_diff_modified() {
        let old = Manifest::new(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: hash_string("old content"),
            mode: FileMode::Regular,
        }]);
        let new = Manifest::new(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: hash_string("new content"),
            mode: FileMode::Regular,
        }]);

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Modified);
    }

    #[test]
    fn test_commit_creation() {
        let commit = Commit::new(
            "main".to_string(),
            None,
            None,
            hash_string("manifest"),
            "author".to_string(),
            Some("test message".to_string()),
            vec![],
        )
        .unwrap();
        assert_eq!(commit.branch_name, "main");
        assert!(commit.parent_hash.is_none());
        assert!(commit.merge_parent_hash.is_none());
    }

    #[test]
    fn test_commit_rejects_control_chars_in_branch_and_author() {
        for (branch, author) in [
            ("main\nevil", "author"),
            ("main\tevil", "author"),
            ("main\0evil", "author"),
            ("main", "author\nevil"),
            ("main", "author\tevil"),
            ("main", "author\0evil"),
        ] {
            let err = Commit::new(
                branch.to_string(),
                None,
                None,
                hash_string("manifest"),
                author.to_string(),
                None,
                vec![],
            )
            .unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "({branch:?}, {author:?}) should be rejected, got {err:?}"
            );
        }
        // Multi-line messages stay legal — the message is the one free-form
        // field, and its neighbors being control-char-free keeps the
        // preimage unambiguous.
        assert!(Commit::new(
            "main".to_string(),
            None,
            None,
            hash_string("manifest"),
            "author".to_string(),
            Some("line one\nline two".to_string()),
            vec![],
        )
        .is_ok());
    }

    #[test]
    fn test_branch_creation() {
        let br = Branch::new("feature-x".to_string(), Some("A feature".to_string()), None);
        assert_eq!(br.name, "feature-x");
        assert_eq!(br.status, BranchStatus::Open);
        assert!(br.parent_branch.is_none());
    }

    #[test]
    fn test_manifest_diff_no_changes() {
        let entries = vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }];
        let m1 = Manifest::new(entries.clone());
        let m2 = Manifest::new(entries);
        let changes = m1.diff(&m2);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_manifest_diff_multiple_changes() {
        let old = Manifest::new(vec![
            ManifestEntry {
                path: "a.txt".to_string(),
                blob_hash: hash_string("a"),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "b.txt".to_string(),
                blob_hash: hash_string("b"),
                mode: FileMode::Regular,
            },
        ]);
        let new = Manifest::new(vec![
            ManifestEntry {
                path: "b.txt".to_string(),
                blob_hash: hash_string("b_modified"),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "c.txt".to_string(),
                blob_hash: hash_string("c"),
                mode: FileMode::Regular,
            },
        ]);
        let changes = old.diff(&new);
        assert_eq!(changes.len(), 3);
        // a.txt deleted, b.txt modified, c.txt added
        assert!(changes
            .iter()
            .any(|c| c.path == "a.txt" && c.change_type == ChangeType::Deleted));
        assert!(changes
            .iter()
            .any(|c| c.path == "b.txt" && c.change_type == ChangeType::Modified));
        assert!(changes
            .iter()
            .any(|c| c.path == "c.txt" && c.change_type == ChangeType::Added));
    }

    #[test]
    fn test_manifest_diff_mode_only_change() {
        let hash = hash_string("content");
        let old = Manifest::new(vec![ManifestEntry {
            path: "script.sh".to_string(),
            blob_hash: hash.clone(),
            mode: FileMode::Regular,
        }]);
        let new = Manifest::new(vec![ManifestEntry {
            path: "script.sh".to_string(),
            blob_hash: hash.clone(),
            mode: FileMode::Executable,
        }]);
        let changes = old.diff(&new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "script.sh");
        assert_eq!(changes[0].change_type, ChangeType::Modified);
        assert_eq!(changes[0].old_blob_hash, Some(hash.clone()));
        assert_eq!(changes[0].new_blob_hash, Some(hash));
    }

    #[test]
    fn test_filemode_default() {
        let mode: FileMode = Default::default();
        assert_eq!(mode, FileMode::Regular);
    }

    #[test]
    fn test_metadata_key_as_str() {
        assert_eq!(MetadataKey::Head.as_str(), "head");
        assert_eq!(MetadataKey::RemoteUrl.as_str(), "remote_url");
        assert_eq!(MetadataKey::RepoName.as_str(), "repo_name");
        assert_eq!(MetadataKey::CurrentBranch.as_str(), "current_branch");
        assert_eq!(MetadataKey::ApiKey.as_str(), "api_key");
        assert_eq!(
            MetadataKey::MainLastCheckedAt.as_str(),
            "main_last_checked_at"
        );
    }

    #[test]
    fn test_metadata_key_display() {
        assert_eq!(MetadataKey::Head.to_string(), "head");
    }

    #[test]
    fn test_change_type_display() {
        assert_eq!(ChangeType::Added.to_string(), "A");
        assert_eq!(ChangeType::Modified.to_string(), "M");
        assert_eq!(ChangeType::Deleted.to_string(), "D");
        assert_eq!(ChangeType::Renamed.to_string(), "R");
    }

    #[test]
    fn test_branch_status_display() {
        assert_eq!(BranchStatus::Open.to_string(), "open");
        assert_eq!(BranchStatus::Closed.to_string(), "closed");
    }

    #[test]
    fn test_manifest_get() {
        let hash = hash_string("content");
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: hash.clone(),
            mode: FileMode::Regular,
        }]);
        assert!(manifest.get("file.txt").is_some());
        assert!(manifest.get("nonexistent.txt").is_none());
    }

    #[test]
    fn test_manifest_empty() {
        let empty = Manifest::empty();
        assert!(empty.entries.is_empty());
    }

    #[test]
    fn test_blob_from_string() {
        let blob = Blob::from_string("hello");
        assert_eq!(blob.content, b"hello");
        assert_eq!(blob.size, 5);
    }

    #[test]
    fn test_manifest_diff_exact_rename() {
        let content_hash = hash_string("same content");
        let old = Manifest::new(vec![ManifestEntry {
            path: "old_name.txt".to_string(),
            blob_hash: content_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let new = Manifest::new(vec![ManifestEntry {
            path: "new_name.txt".to_string(),
            blob_hash: content_hash,
            mode: FileMode::Regular,
        }]);

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Renamed);
        assert_eq!(changes[0].path, "new_name.txt");
        assert_eq!(changes[0].old_path.as_deref(), Some("old_name.txt"));
    }

    #[test]
    fn test_manifest_diff_only_exact_renames() {
        // Manifest-level diff only catches identical-hash renames.
        // Content-similarity rename detection lives in detect_renames_by_content.
        let old = Manifest::new(vec![ManifestEntry {
            path: "src/player.rs".to_string(),
            blob_hash: hash_string("old code"),
            mode: FileMode::Regular,
        }]);
        let new = Manifest::new(vec![ManifestEntry {
            path: "lib/player.rs".to_string(),
            blob_hash: hash_string("new code"),
            mode: FileMode::Regular,
        }]);

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Deleted));
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Added));
    }

    #[test]
    fn test_manifest_diff_exact_renames_with_duplicate_hashes() {
        let shared = hash_string("same content");
        let old = Manifest::new(vec![
            ManifestEntry {
                path: "old/a.txt".to_string(),
                blob_hash: shared.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "old/b.txt".to_string(),
                blob_hash: shared.clone(),
                mode: FileMode::Executable,
            },
            ManifestEntry {
                path: "old/deleted.txt".to_string(),
                blob_hash: hash_string("deleted"),
                mode: FileMode::Regular,
            },
        ]);
        let new = Manifest::new(vec![
            ManifestEntry {
                path: "new/a.txt".to_string(),
                blob_hash: shared.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "new/b.txt".to_string(),
                blob_hash: shared,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "new/added.txt".to_string(),
                blob_hash: hash_string("added"),
                mode: FileMode::Regular,
            },
        ]);

        let changes = old.diff(&new);
        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.change_type == ChangeType::Renamed)
                .count(),
            2
        );
        assert!(changes
            .iter()
            .any(|c| c.path == "old/deleted.txt" && c.change_type == ChangeType::Deleted));
        assert!(changes
            .iter()
            .any(|c| c.path == "new/added.txt" && c.change_type == ChangeType::Added));
    }

    #[test]
    fn test_content_similarity_identical() {
        let bytes = b"line one\nline two\nline three";
        assert!((content_similarity(bytes, bytes) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_content_similarity_partial_overlap() {
        let old = b"line one\nline two\nline three\nline four";
        let new = b"line one\nline two\nline three\nline five";
        // 3 shared lines, 5 unique lines total -> 3/5 = 0.6
        let s = content_similarity(old, new);
        assert!((s - 0.6).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn test_content_similarity_disjoint() {
        let old = b"alpha\nbeta\ngamma";
        let new = b"one\ntwo\nthree";
        assert_eq!(content_similarity(old, new), 0.0);
    }

    #[test]
    fn test_content_similarity_binary_returns_zero() {
        // NUL byte in input -> treated as binary -> 0.0
        let old = b"some\0binary";
        let new = b"some\0binary";
        assert_eq!(content_similarity(old, new), 0.0);
    }

    #[test]
    fn test_content_similarity_empty_returns_zero() {
        assert_eq!(content_similarity(b"", b"anything"), 0.0);
        assert_eq!(content_similarity(b"anything", b""), 0.0);
    }

    #[test]
    fn test_detect_renames_by_content_promotes_pair() {
        let del_hash = hash_string("old code");
        let add_hash = hash_string("edited code");
        let mut changes = vec![
            FileChange {
                path: "src/foo.rs".to_string(),
                change_type: ChangeType::Deleted,
                old_blob_hash: Some(del_hash.clone()),
                new_blob_hash: None,
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
            FileChange {
                path: "lib/foo.rs".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(add_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
        ];

        // Old has 4 lines; new shares 3 of them (60% Jaccard) → above 0.5 default.
        let del_bytes = b"alpha\nbeta\ngamma\ndelta".to_vec();
        let add_bytes = b"alpha\nbeta\ngamma\nepsilon".to_vec();

        let get_content = |h: &Hash| -> Result<Option<Vec<u8>>> {
            if *h == del_hash {
                Ok(Some(del_bytes.clone()))
            } else if *h == add_hash {
                Ok(Some(add_bytes.clone()))
            } else {
                Ok(None)
            }
        };

        detect_renames_by_content(&mut changes, get_content, &RenameConfig::default()).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Renamed);
        assert_eq!(changes[0].path, "lib/foo.rs");
        assert_eq!(changes[0].old_path.as_deref(), Some("src/foo.rs"));
    }

    #[test]
    fn test_detect_renames_by_content_skips_dissimilar() {
        let del_hash = hash_string("a");
        let add_hash = hash_string("b");
        let mut changes = vec![
            FileChange {
                path: "old.rs".to_string(),
                change_type: ChangeType::Deleted,
                old_blob_hash: Some(del_hash.clone()),
                new_blob_hash: None,
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
            FileChange {
                path: "new.rs".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(add_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
        ];

        let get_content = |h: &Hash| -> Result<Option<Vec<u8>>> {
            if *h == del_hash {
                Ok(Some(b"one\ntwo\nthree".to_vec()))
            } else {
                Ok(Some(b"forty\nfifty\nsixty".to_vec()))
            }
        };

        detect_renames_by_content(&mut changes, get_content, &RenameConfig::default()).unwrap();

        // Below threshold — original Added/Deleted pair preserved.
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Added));
        assert!(changes.iter().any(|c| c.change_type == ChangeType::Deleted));
    }

    #[test]
    fn test_detect_renames_by_content_greedy_best_match() {
        // Two adds, one delete. The add with higher similarity should win.
        let del_hash = hash_string("d");
        let add_hash_high = hash_string("ah");
        let add_hash_low = hash_string("al");
        let mut changes = vec![
            FileChange {
                path: "src/lib.rs".to_string(),
                change_type: ChangeType::Deleted,
                old_blob_hash: Some(del_hash.clone()),
                new_blob_hash: None,
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
            FileChange {
                path: "src/match_high.rs".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(add_hash_high.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
            FileChange {
                path: "src/match_low.rs".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(add_hash_low.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
        ];

        let get_content = |h: &Hash| -> Result<Option<Vec<u8>>> {
            if *h == del_hash {
                Ok(Some(b"a\nb\nc\nd".to_vec()))
            } else if *h == add_hash_high {
                Ok(Some(b"a\nb\nc\nz".to_vec())) // 3/5 = 0.6
            } else {
                Ok(Some(b"a\nx\ny\nz".to_vec())) // 1/7 ≈ 0.14
            }
        };

        detect_renames_by_content(&mut changes, get_content, &RenameConfig::default()).unwrap();

        // The high-similarity add gets paired as Renamed; the low one stays Added.
        let renamed: Vec<_> = changes
            .iter()
            .filter(|c| c.change_type == ChangeType::Renamed)
            .collect();
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].path, "src/match_high.rs");
        assert!(changes
            .iter()
            .any(|c| c.path == "src/match_low.rs" && c.change_type == ChangeType::Added));
    }

    #[test]
    fn test_detect_renames_by_content_respects_max_candidates() {
        // With max_candidates = 1, pass refuses to run when D + A > 1.
        let del_hash = hash_string("d");
        let add_hash = hash_string("a");
        let mut changes = vec![
            FileChange {
                path: "old.rs".to_string(),
                change_type: ChangeType::Deleted,
                old_blob_hash: Some(del_hash.clone()),
                new_blob_hash: None,
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
            FileChange {
                path: "new.rs".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(add_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: None,
            },
        ];

        let get_content =
            |_: &Hash| -> Result<Option<Vec<u8>>> { Ok(Some(b"a\nb\nc\nd\ne".to_vec())) };

        let config = RenameConfig {
            max_candidates: 1,
            ..RenameConfig::default()
        };
        detect_renames_by_content(&mut changes, get_content, &config).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(!changes.iter().any(|c| c.change_type == ChangeType::Renamed));
    }
}
