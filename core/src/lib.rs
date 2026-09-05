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
/// Sparse-checkout cone matching (Perforce-style partial clones). Pure path
/// logic with no I/O, so it is available to both the CLI client and any
/// server-side consumer without the `local-repo` feature.
pub mod sparse;
pub mod tree;

// Client-side local repository, gated behind the default `local-repo` feature.
#[cfg(feature = "local-repo")]
pub mod git;
#[cfg(feature = "local-repo")]
pub mod sqlite;
#[cfg(feature = "local-repo")]
pub mod traits;

pub use capabilities::Capabilities;
pub use chunking::{chunk_content, reassemble_chunks, stream_chunk_content, ChunkInfo};
#[cfg(feature = "local-repo")]
pub use chunking::{chunk_decode, chunk_encode};
pub use diff::{
    binary_or_large_notice, binary_or_oversize_notice, inline_diff_ranges, is_binary, DiffLine,
    FileDiff, FileStatus, Hunk, DEFAULT_CONTEXT_LINES, MAX_TEXT_DIFF_BYTES,
};
pub use engine::{detect_engine, GameEngine};
pub use error::{CommitPhaseError, FinishPhaseError, FinishPreflightError, OakError, Result};
pub use hash::{hash_bytes, hash_string, Hash, HashKind};
pub use hash_format::HashFormat;
pub use ignore::{is_system_metadata_path, IgnorePatterns};
pub use merge::{
    three_way_merge_manifests, three_way_merge_text, MergeConflict, MergeOutcome, TextMerge,
};
pub use sparse::SparseCone;
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
pub use sqlite::{BulkImporter, CompactStats, ServeMappingProofRecord, SqliteRepository};
#[cfg(feature = "local-repo")]
pub use traits::{BlobSource, Repository, StatCacheEntry};

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

    /// The zero-byte blob — every empty file in every repo shares this one
    /// object.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Hash of zero-byte content (`af1349b9…`, BLAKE3 of the empty input).
    ///
    /// Under the v1 content-addressing format this value is *also*
    /// [`Tree::empty_hash`] and the hash of an empty chunk, because v1 hashes
    /// every object kind's raw bytes with no domain prefix — see
    /// [`hash_format`] for why that is a weakness v2 fixes. The three values
    /// coincide today but the *meanings* never did, so blob-side code must
    /// use this constant and tree-side code `Tree::empty_hash`: under a
    /// domain-separated format they diverge and both call sites stay correct.
    pub fn empty_hash() -> Hash {
        hash_format::hash_blob(HashFormat::V1, b"")
    }

    /// Is `hash` the hash of zero-byte content?
    ///
    /// This is the one blob hash whose content is fully determined by the
    /// hash itself, which is what makes [`ensure_empty_blob`] sound.
    pub fn is_empty_hash(hash: &Hash) -> bool {
        hash == &Self::empty_hash()
    }
}

/// Store the zero-byte blob locally if `hash` names it; report whether it did.
///
/// A blob missing from local storage normally means data loss — the bytes
/// exist only where they were pushed, and writing a file without them would
/// produce a working tree that silently disagrees with its manifest. The
/// empty blob is the single exception: its content is *implied* by its hash
/// (there is exactly one byte string that hashes to [`Blob::empty_hash`], and
/// it is the empty one), so the client can always reconstruct it without the
/// server having shipped anything.
///
/// That exception is load-bearing in practice. Servers whose blob storage was
/// migrated with a `octet_length(content) > 0` filter have a metadata-only
/// row for the empty blob: `blobs/check` reports it present while pull omits
/// it, so the user can neither clone nor repair by re-pushing. Synthesizing
/// it client-side unblocks those repos without weakening the missing-blob
/// protection for any hash whose bytes are *not* derivable.
///
/// Returns `false` — without touching storage — for any other hash, so
/// callers can keep their existing hard error in the `false` arm.
#[cfg(feature = "local-repo")]
pub fn ensure_empty_blob(repo: &dyn Repository, hash: &Hash) -> Result<bool> {
    if !Blob::is_empty_hash(hash) {
        return Ok(false);
    }
    repo.store_blob(&Blob::empty())?;
    Ok(true)
}

/// Run [`ensure_empty_blob`] for every entry of `manifest`, so a subsequent
/// "is every blob present?" sweep sees the empty blob as available.
///
/// Cheap when there is nothing to do: manifests that reference no empty file
/// never touch storage, and one that references several (two empty files in
/// different directories are the *same* blob) writes once.
#[cfg(feature = "local-repo")]
pub fn ensure_empty_blobs_in_manifest(repo: &dyn Repository, manifest: &Manifest) -> Result<()> {
    if manifest
        .entries
        .iter()
        .any(|e| Blob::is_empty_hash(&e.blob_hash))
    {
        repo.store_blob(&Blob::empty())?;
    }
    Ok(())
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
        self.diff_entries_with_renames(&other.entries)
    }

    /// Compare with another manifest with *full* rename detection: exact
    /// blob-hash renames from [`Manifest::diff_with_renames`], then
    /// content-similarity promotion of remaining Delete+Add pairs via
    /// [`detect_renames_by_content`].
    ///
    /// This is the diff every user-facing change set should go through so
    /// `oak diff`, `oak branch diff`, `oak status`, and `oak commit` agree on
    /// rename-with-edit. `get_content` loads blob bytes by hash (returning
    /// `Ok(None)` for missing blobs, which skips that candidate); the
    /// `config` caps (`max_file_bytes`, `max_candidates`) bound how much
    /// content is ever loaded.
    pub fn diff_with_content_renames<F>(
        &self,
        other: &Manifest,
        get_content: F,
        config: &RenameConfig,
    ) -> Result<Vec<FileChange>>
    where
        F: FnMut(&Hash) -> Result<Option<Vec<u8>>>,
    {
        self.diff_entries_with_content_renames(&other.entries, get_content, config)
    }

    /// Entries-based variant of [`Manifest::diff_with_content_renames`], for
    /// status-like callers that already resolved the working tree to manifest
    /// entries (see [`Manifest::diff_entries_with_renames`]).
    pub fn diff_entries_with_content_renames<F>(
        &self,
        other_entries: &[ManifestEntry],
        get_content: F,
        config: &RenameConfig,
    ) -> Result<Vec<FileChange>>
    where
        F: FnMut(&Hash) -> Result<Option<Vec<u8>>>,
    {
        let mut changes = self.diff_entries_with_renames(other_entries);
        detect_renames_by_content(&mut changes, get_content, config)?;
        Ok(changes)
    }

    /// Compare with a set of entries without first constructing a full
    /// [`Manifest`] for them.
    ///
    /// This is useful for status-like callers that already resolved the working
    /// tree to manifest entries and only need the change set. It avoids sorting
    /// and hashing the entire new tree before diffing, while preserving the same
    /// exact-hash rename behavior as [`Manifest::diff_with_renames`].
    pub fn diff_entries_with_renames(&self, other_entries: &[ManifestEntry]) -> Vec<FileChange> {
        let mut changes = Vec::new();
        let mut deleted: Vec<(&str, &ManifestEntry)> = Vec::new();
        let mut added: Vec<(&str, &ManifestEntry)> = Vec::new();

        // Build one lookup map for the old manifest, then classify each current
        // entry as added/modified while recording seen paths. The older
        // implementation also built a full new-manifest path map; status callers
        // already have the current entries, so that extra map and full-manifest
        // construction are avoidable.
        let self_map: HashMap<&str, &ManifestEntry> =
            self.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        let mut seen_other: HashSet<&str> = HashSet::with_capacity(other_entries.len());

        // Find added and modified files.
        for other_entry in other_entries {
            let path = other_entry.path.as_str();
            seen_other.insert(path);
            match self_map.get(path) {
                None => {
                    added.push((path, other_entry));
                }
                Some(entry) => {
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

        // Find removed files.
        for entry in &self.entries {
            let path = entry.path.as_str();
            if !seen_other.contains(path) {
                deleted.push((path, entry));
            }
        }
        added.sort_by(|(a, _), (b, _)| a.cmp(b));

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

/// Validate that a commit's file-change records describe exactly the
/// transition from `parent` to `current`.
///
/// The representation is semantic: a rename may change content and mode, and
/// independent records may form an overwrite or rename cycle. Validation is
/// therefore order-independent: every record claims an immutable old-side
/// parent path and/or new-side current path, and those claims must exactly
/// cover the paths whose entries changed.
pub fn validate_manifest_transition(
    parent: &Manifest,
    current: &Manifest,
    changes: &[FileChange],
) -> std::result::Result<(), String> {
    let parent_entries: std::collections::BTreeMap<String, (Hash, FileMode)> = parent
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), (entry.blob_hash.clone(), entry.mode)))
        .collect();
    let current_entries: std::collections::BTreeMap<String, (Hash, FileMode)> = current
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), (entry.blob_hash.clone(), entry.mode)))
        .collect();
    if parent_entries.len() != parent.entries.len()
        || current_entries.len() != current.entries.len()
    {
        return Err("manifest contains duplicate paths".to_string());
    }

    let expected_old: std::collections::BTreeSet<String> = parent_entries
        .iter()
        .filter(|(path, entry)| current_entries.get(*path) != Some(*entry))
        .map(|(path, _)| path.clone())
        .collect();
    let expected_new: std::collections::BTreeSet<String> = current_entries
        .iter()
        .filter(|(path, entry)| parent_entries.get(*path) != Some(*entry))
        .map(|(path, _)| path.clone())
        .collect();
    let mut claimed_old = std::collections::BTreeSet::new();
    let mut claimed_new = std::collections::BTreeSet::new();

    let claim_old = |path: &str,
                     hash: Option<&Hash>,
                     mode: Option<FileMode>,
                     claimed: &mut std::collections::BTreeSet<String>|
     -> std::result::Result<(), String> {
        let hash = hash.ok_or_else(|| format!("old-side path {path:?} has no blob"))?;
        let entry = parent_entries
            .get(path)
            .ok_or_else(|| format!("old-side path {path:?} is absent from parent"))?;
        if entry.0 != *hash || mode.is_some_and(|claimed_mode| claimed_mode != entry.1) {
            return Err(format!(
                "old-side path {path:?} claims blob {hash} but does not match parent blob {}",
                entry.0
            ));
        }
        if !claimed.insert(path.to_string()) {
            return Err(format!("old-side path {path:?} is claimed more than once"));
        }
        Ok(())
    };
    let claim_new = |path: &str,
                     hash: Option<&Hash>,
                     mode: Option<FileMode>,
                     claimed: &mut std::collections::BTreeSet<String>|
     -> std::result::Result<(), String> {
        let hash = hash.ok_or_else(|| format!("new-side path {path:?} has no blob"))?;
        let entry = current_entries
            .get(path)
            .ok_or_else(|| format!("new-side path {path:?} is absent from current"))?;
        if entry.0 != *hash || mode.is_some_and(|claimed_mode| claimed_mode != entry.1) {
            return Err(format!(
                "new-side path {path:?} claims blob {hash} but does not match current blob {}",
                entry.0
            ));
        }
        if !claimed.insert(path.to_string()) {
            return Err(format!("new-side path {path:?} is claimed more than once"));
        }
        Ok(())
    };

    for change in changes {
        match change.change_type {
            ChangeType::Added => {
                if change.old_blob_hash.is_some()
                    || change.old_path.is_some()
                    || change.old_mode.is_some()
                {
                    return Err(format!(
                        "addition {:?} has invalid old-side state",
                        change.path
                    ));
                }
                claim_new(
                    &change.path,
                    change.new_blob_hash.as_ref(),
                    change.new_mode,
                    &mut claimed_new,
                )?;
            }
            ChangeType::Deleted => {
                if change.new_blob_hash.is_some()
                    || change.old_path.is_some()
                    || change.new_mode.is_some()
                {
                    return Err(format!(
                        "deletion {:?} has invalid new-side state",
                        change.path
                    ));
                }
                claim_old(
                    &change.path,
                    change.old_blob_hash.as_ref(),
                    change.old_mode,
                    &mut claimed_old,
                )?;
            }
            ChangeType::Renamed => {
                let old_path = change
                    .old_path
                    .as_ref()
                    .ok_or_else(|| format!("rename {:?} has no old_path", change.path))?;
                if old_path == &change.path {
                    return Err(format!(
                        "rename source and destination are both {old_path:?}"
                    ));
                }
                claim_old(
                    old_path,
                    change.old_blob_hash.as_ref(),
                    change.old_mode,
                    &mut claimed_old,
                )?;
                claim_new(
                    &change.path,
                    change.new_blob_hash.as_ref(),
                    change.new_mode,
                    &mut claimed_new,
                )?;
            }
            ChangeType::Modified => {
                if change.old_path.is_some() {
                    return Err(format!(
                        "modification {:?} must not carry old_path",
                        change.path
                    ));
                }
                claim_old(
                    &change.path,
                    change.old_blob_hash.as_ref(),
                    change.old_mode,
                    &mut claimed_old,
                )?;
                claim_new(
                    &change.path,
                    change.new_blob_hash.as_ref(),
                    change.new_mode,
                    &mut claimed_new,
                )?;
            }
        }
    }

    if claimed_old != expected_old || claimed_new != expected_new {
        return Err("file changes do not describe the complete manifest transition".to_string());
    }
    Ok(())
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
    /// Create a new commit, computing the hash.
    ///
    /// The timestamp is truncated to microsecond precision before hashing.
    /// The hash preimage embeds `timestamp.to_rfc3339()`, and the
    /// lowest-precision store a commit row must survive is the server's
    /// Postgres `TIMESTAMPTZ` (microseconds) — a nanosecond-precision
    /// timestamp (Linux `Utc::now()`) would hash all nine digits, get
    /// truncated on store, and leave a row that can never reproduce its own
    /// hash. See [`Commit::rehydrate_verified`] for how already-stored
    /// nanosecond-hashed commits are still verified.
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
            truncate_to_micros(Utc::now()),
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

    /// Reconstruct a commit from stored/wire fields, verifying that the
    /// fields reproduce `claimed_hash` — with tolerance for
    /// microsecond-truncated timestamps.
    ///
    /// Server-side Postgres stores commit timestamps as `TIMESTAMPTZ`
    /// (microsecond precision), but every commit created on a
    /// nanosecond-resolution clock before [`Commit::new`] started truncating
    /// (every squash-merge the Linux server ever made) hashed all nine
    /// subsecond digits into its preimage. The stored row therefore can't
    /// reproduce its own hash from its own fields.
    ///
    /// When the exact fields don't reproduce `claimed_hash` and the
    /// timestamp carries no sub-microsecond digits, this restores each
    /// possible lost nanosecond tail (`1..=999`) and accepts the one that
    /// reproduces the claimed hash — recovering, not trusting, the original
    /// preimage: a match proves the fields are byte-identical to what was
    /// hashed, minus precision the store dropped. The returned commit
    /// carries the recovered timestamp, so re-storing it locally yields an
    /// exactly self-verifying row.
    ///
    /// A second legacy class is also tolerated: commits hashed before
    /// `merge_parent_hash` entered the v1 preimage verify against the frozen
    /// 6-field preimage ([`hash_format::hash_commit_v1_pre_merge_parent`]),
    /// strictly gated on the commit having no merge parent.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate_verified(
        claimed_hash: &Hash,
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

        let fields = hash_format::CommitFields {
            branch_name: &branch_name,
            parent_hash: parent_hash.as_ref(),
            merge_parent_hash: merge_parent_hash.as_ref(),
            manifest_hash: &manifest_hash,
            author: &author,
            message: message.as_deref(),
            timestamp: &timestamp,
            files: &files,
        };
        let exact = hash_format::hash_commit(hash_format::HashFormat::V1, &fields);

        let verified_timestamp = if &exact == claimed_hash {
            Some(timestamp)
        } else if timestamp.timestamp_subsec_nanos().is_multiple_of(1_000) {
            // Microsecond-precision timestamp: try restoring a truncated
            // nanosecond tail. ~1000 BLAKE3 hashes of a few hundred bytes —
            // well under a millisecond, and only on the mismatch path.
            (1..1_000).find_map(|tail| {
                let candidate = timestamp + chrono::Duration::nanoseconds(tail);
                let hash = hash_format::hash_commit(
                    hash_format::HashFormat::V1,
                    &hash_format::CommitFields {
                        timestamp: &candidate,
                        ..fields
                    },
                );
                (&hash == claimed_hash).then_some(candidate)
            })
        } else {
            None
        };

        // Second legacy class: commits hashed before `merge_parent_hash`
        // entered the v1 preimage (~April 2026 and earlier) used a 6-field
        // preimage with no merge_parent line at all. Their timestamps
        // reproduce exactly, so no nanosecond-tail search is needed here.
        //
        // Strictly gated on `merge_parent_hash.is_none()`: every commit of
        // that era has no merge parent, and the gate removes the only
        // theoretical two-preimage ambiguity — a commit *with* a merge
        // parent can only ever verify under today's 7-field rule. Accepting
        // the 6-field match is safe because both preimages are versions of
        // the same trusted v1 scheme over the same fields; a match still
        // proves the fields are byte-identical to what was originally
        // hashed.
        let verified_timestamp = verified_timestamp.or_else(|| {
            (merge_parent_hash.is_none()
                && &hash_format::hash_commit_v1_pre_merge_parent(&fields) == claimed_hash)
                .then_some(timestamp)
        });

        let Some(timestamp) = verified_timestamp else {
            return Err(OakError::InvalidHash(format!(
                "commit hash mismatch: peer claimed {claimed_hash}, canonical fields hash to {exact}"
            )));
        };

        Ok(Commit {
            hash: claimed_hash.clone(),
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

/// Truncate a timestamp to microsecond precision — the precision of the
/// lowest-fidelity store a commit row must survive (Postgres `TIMESTAMPTZ`).
/// Hash preimages must only ever embed timestamps every store can reproduce.
pub fn truncate_to_micros(ts: DateTime<Utc>) -> DateTime<Utc> {
    ts - chrono::Duration::nanoseconds((ts.timestamp_subsec_nanos() % 1_000) as i64)
}

/// Reject control characters in a commit-hash preimage field. The preimage is
/// `\n`-joined, so control characters here can make serialized commit metadata
/// ambiguous or non-printable. See [`Commit::with_timestamp`].
fn validate_commit_field(field: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(OakError::InvalidArgument(format!(
            "commit {field} {value:?} contains a control character"
        )));
    }
    Ok(())
}

/// Explicit audit reason recorded when a branch is closed via `oak close`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    Superseded,
    Stale,
    Duplicate,
    Other(String),
}

impl CloseReason {
    pub fn as_str(&self) -> &str {
        match self {
            CloseReason::Superseded => "superseded",
            CloseReason::Stale => "stale",
            CloseReason::Duplicate => "duplicate",
            CloseReason::Other(reason) => reason.as_str(),
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        if s.trim() != s || s.is_empty() || s.chars().any(char::is_control) {
            return Err(OakError::InvalidArgument(
                "close reason cannot be empty, have leading/trailing whitespace, or contain control characters"
                    .to_string(),
            ));
        }
        match s {
            "superseded" => Ok(CloseReason::Superseded),
            "stale" => Ok(CloseReason::Stale),
            "duplicate" => Ok(CloseReason::Duplicate),
            _ => Ok(CloseReason::Other(s.to_string())),
        }
    }
}

impl Serialize for CloseReason {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CloseReason {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        CloseReason::parse(&value).map_err(serde::de::Error::custom)
    }
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
    /// User/agent-provided close reason; never inferred from prose.
    #[serde(default)]
    pub close_reason: Option<CloseReason>,
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
            close_reason: None,
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
    /// Non-empty when the last attempted verified fetch of server-owned `main`
    /// failed integrity checks. Checkout-free review/diff must not silently use
    /// stale local `main` evidence while this is set.
    MainLastFetchIntegrityError,
    /// Repo-level content-addressing format (see [`crate::hash_format`]).
    /// Absent means v1; nothing writes this key yet — v2 is prepared but
    /// cannot be enabled until an explicit migration ships.
    HashFormat,
    /// Active sparse-checkout cone: a newline-separated list of path prefixes
    /// the working tree is scoped to (see [`crate::sparse`]). Absent or empty
    /// means a full (non-sparse) checkout. Written by `oak clone --path` and
    /// `oak sparse`.
    SparsePaths,
    /// Blob hashes the server has withheld from this clone because path-based
    /// permissions deny the user (newline-separated). Recorded from pull/clone
    /// responses so status/commit can tell "restricted, never materialized"
    /// apart from a real deletion. A hash that later becomes locally present
    /// (access granted, then pulled) is simply ignored by consumers — they
    /// check blob absence before treating a path as restricted.
    RestrictedBlobs,
    /// Blob hashes whose bytes an operator explicitly adjudicated as
    /// permanently unavailable legacy content. Kept separate from access
    /// restrictions so the repository remains visibly degraded.
    KnownLostBlobs,
    /// Internal `oak serve` receipt set for commits published through the
    /// headless staged-v1 bootstrap protocol. Never exposed as branch state.
    StagedPushV1Sessions,
    /// Durable paged async-v1 mapping proof jobs used by `oak serve`.
    ServeMappingProofJobs,
    /// Monotonic cache of immutable objects that have been made reachable by
    /// an `oak serve` branch head. A superset is safe because hashes are
    /// immutable; it avoids rescanning all published history on every stage.
    ServePublishedClosure,
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
            MetadataKey::MainLastFetchIntegrityError => "main_last_fetch_integrity_error",
            MetadataKey::HashFormat => "hash_format",
            MetadataKey::SparsePaths => "sparse_paths",
            MetadataKey::RestrictedBlobs => "restricted_blobs",
            MetadataKey::KnownLostBlobs => "known_lost_blobs",
            MetadataKey::StagedPushV1Sessions => "staged_push_v1_sessions",
            MetadataKey::ServeMappingProofJobs => "serve_mapping_proof_jobs",
            MetadataKey::ServePublishedClosure => "serve_published_closure",
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

    /// `ensure_empty_blob` reconstructs exactly one blob and never guesses at
    /// any other: a hash whose bytes aren't implied by the hash must be left
    /// missing so the caller's hard error still fires.
    #[cfg(feature = "local-repo")]
    #[test]
    fn ensure_empty_blob_only_synthesizes_the_empty_blob() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&dir.path().join("t.db")).unwrap();

        let absent = Blob::from_str("bytes only the pusher ever had").hash;
        assert!(!ensure_empty_blob(&repo, &absent).unwrap());
        assert!(!repo.has_blob(&absent).unwrap());

        assert!(ensure_empty_blob(&repo, &Blob::empty_hash()).unwrap());
        let stored = repo.get_blob(&Blob::empty_hash()).unwrap().unwrap();
        assert_eq!(stored.size, 0);
        assert!(stored.content.is_empty());
        // Idempotent — a second call is a harmless re-insert, not an error.
        assert!(ensure_empty_blob(&repo, &Blob::empty_hash()).unwrap());
    }

    /// Two empty files in different directories share one blob, so a manifest
    /// sweep writes it once and leaves ordinary entries alone.
    #[cfg(feature = "local-repo")]
    #[test]
    fn ensure_empty_blobs_in_manifest_covers_deduped_entries() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&dir.path().join("t.db")).unwrap();

        let other = Blob::from_str("real content").hash;
        let manifest = Manifest::new(vec![
            ManifestEntry {
                path: "docs/EMPTY".to_string(),
                blob_hash: Blob::empty_hash(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "src/EMPTY".to_string(),
                blob_hash: Blob::empty_hash(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "README.md".to_string(),
                blob_hash: other.clone(),
                mode: FileMode::Regular,
            },
        ]);

        ensure_empty_blobs_in_manifest(&repo, &manifest).unwrap();
        assert!(repo.has_blob(&Blob::empty_hash()).unwrap());
        assert!(
            !repo.has_blob(&other).unwrap(),
            "only the empty blob is derivable; everything else must stay missing"
        );
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
            ("main\u{1b}evil", "author"),
            ("main\u{7f}evil", "author"),
            ("main", "author\nevil"),
            ("main", "author\tevil"),
            ("main", "author\0evil"),
            ("main", "author\u{1b}evil"),
            ("main", "author\u{7f}evil"),
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

    /// A pre-merge_parent-era commit (6-field legacy v1 preimage, no merge
    /// parent) must rehydrate against its claimed hash — the fix that lets
    /// post-verification binaries clone repos rooted in April-2026-era
    /// history (e.g. oak/oakspace's root commit f15a1608).
    #[test]
    fn test_rehydrate_accepts_legacy_pre_merge_parent_preimage() {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-12T09:30:00.123456+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let parent = hash_string("parent commit");
        let manifest = hash_string("manifest");

        // The frozen 6-field legacy preimage, rendered by hand so the test
        // pins the byte format independently of the production hasher.
        let legacy_preimage = format!(
            "main\n{}\n{}\nauthor@example.com\na legacy commit\n{}",
            parent,
            manifest,
            timestamp.to_rfc3339()
        );
        let claimed = hash_bytes(legacy_preimage.as_bytes());
        assert_eq!(
            claimed,
            hash_format::hash_commit_v1_pre_merge_parent(&hash_format::CommitFields {
                branch_name: "main",
                parent_hash: Some(&parent),
                merge_parent_hash: None,
                manifest_hash: &manifest,
                author: "author@example.com",
                message: Some("a legacy commit"),
                timestamp: &timestamp,
                files: &[],
            }),
            "legacy hasher must reproduce the hand-rendered 6-field preimage"
        );

        // No merge parent → accepted, claimed hash and timestamp preserved.
        let commit = Commit::rehydrate_verified(
            &claimed,
            "main".to_string(),
            Some(parent.clone()),
            None,
            manifest.clone(),
            "author@example.com".to_string(),
            Some("a legacy commit".to_string()),
            vec![],
            timestamp,
        )
        .expect("legacy 6-field commit without merge parent must verify");
        assert_eq!(commit.hash, claimed);
        assert_eq!(commit.timestamp, timestamp);

        // With a merge parent the legacy path is gated off: the same claimed
        // hash must be rejected even though the 6-field render ignores the
        // merge parent entirely.
        let err = Commit::rehydrate_verified(
            &claimed,
            "main".to_string(),
            Some(parent.clone()),
            Some(hash_string("merge parent")),
            manifest.clone(),
            "author@example.com".to_string(),
            Some("a legacy commit".to_string()),
            vec![],
            timestamp,
        )
        .expect_err("legacy preimage must not verify a commit with a merge parent");
        assert!(
            matches!(&err, OakError::InvalidHash(m) if m.contains("commit hash mismatch")),
            "expected a commit hash mismatch, got {err:?}"
        );

        // A claimed hash matching neither preimage is still rejected.
        let err = Commit::rehydrate_verified(
            &hash_string("some unrelated hash"),
            "main".to_string(),
            Some(parent),
            None,
            manifest,
            "author@example.com".to_string(),
            Some("a legacy commit".to_string()),
            vec![],
            timestamp,
        )
        .expect_err("a hash matching neither preimage must be rejected");
        assert!(
            matches!(&err, OakError::InvalidHash(m) if m.contains("commit hash mismatch")),
            "expected a commit hash mismatch, got {err:?}"
        );
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
        assert_eq!(
            MetadataKey::MainLastFetchIntegrityError.as_str(),
            "main_last_fetch_integrity_error"
        );
        assert_eq!(MetadataKey::RestrictedBlobs.as_str(), "restricted_blobs");
        assert_eq!(MetadataKey::KnownLostBlobs.as_str(), "known_lost_blobs");
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
    fn test_manifest_diff_entries_matches_manifest_diff_without_new_manifest() {
        let unchanged = hash_string("unchanged");
        let edited_old = hash_string("old");
        let edited_new = hash_string("new");
        let renamed = hash_string("renamed");
        let old = Manifest::new(vec![
            ManifestEntry {
                path: "a.txt".to_string(),
                blob_hash: unchanged.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "b.txt".to_string(),
                blob_hash: edited_old,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "gone.txt".to_string(),
                blob_hash: hash_string("gone"),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "old-name.txt".to_string(),
                blob_hash: renamed.clone(),
                mode: FileMode::Regular,
            },
        ]);
        let scanned_entries = vec![
            ManifestEntry {
                path: "new-name.txt".to_string(),
                blob_hash: renamed,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "added.txt".to_string(),
                blob_hash: hash_string("added"),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "b.txt".to_string(),
                blob_hash: edited_new,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "a.txt".to_string(),
                blob_hash: unchanged,
                mode: FileMode::Regular,
            },
        ];
        let new = Manifest::new(scanned_entries.clone());

        let expected = old.diff(&new);
        let actual = old.diff_entries_with_renames(&scanned_entries);
        assert_eq!(
            expected
                .iter()
                .map(|c| (&c.path, c.change_type, c.old_path.as_deref()))
                .collect::<Vec<_>>(),
            actual
                .iter()
                .map(|c| (&c.path, c.change_type, c.old_path.as_deref()))
                .collect::<Vec<_>>()
        );
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

    /// Build a manifest from `(path, content)` pairs plus a blob store the
    /// content-rename diff can load from.
    fn manifest_with_blobs(files: &[(&str, &str)]) -> (Manifest, HashMap<Hash, Vec<u8>>) {
        let mut blobs = HashMap::new();
        let entries = files
            .iter()
            .map(|(path, content)| {
                let blob_hash = hash_string(content);
                blobs.insert(blob_hash.clone(), content.as_bytes().to_vec());
                ManifestEntry {
                    path: (*path).to_string(),
                    blob_hash,
                    mode: FileMode::Regular,
                }
            })
            .collect();
        (Manifest::new(entries), blobs)
    }

    fn blob_loader(
        blobs: &HashMap<Hash, Vec<u8>>,
    ) -> impl FnMut(&Hash) -> Result<Option<Vec<u8>>> + '_ {
        move |h: &Hash| Ok(blobs.get(h).cloned())
    }

    #[test]
    fn test_diff_with_content_renames_detects_rename_with_edit() {
        // 5 of 6 lines shared -> similarity 5/7 ≈ 0.71, above the 0.5 default.
        let (old, mut blobs) = manifest_with_blobs(&[(
            "src/old_name.rs",
            "line one\nline two\nline three\nline four\nline five\nline six\n",
        )]);
        let (new, new_blobs) = manifest_with_blobs(&[(
            "src/new_name.rs",
            "line one\nline two\nline three\nline four\nline five\nline changed\n",
        )]);
        blobs.extend(new_blobs);

        let changes = old
            .diff_with_content_renames(&new, blob_loader(&blobs), &RenameConfig::default())
            .unwrap();

        assert_eq!(changes.len(), 1, "expected one rename, got {changes:?}");
        assert_eq!(changes[0].change_type, ChangeType::Renamed);
        assert_eq!(changes[0].path, "src/new_name.rs");
        assert_eq!(changes[0].old_path.as_deref(), Some("src/old_name.rs"));
        assert!(changes[0].old_blob_hash.is_some());
        assert!(changes[0].new_blob_hash.is_some());
    }

    #[test]
    fn semantic_manifest_transition_accepts_rename_with_edit() {
        let old_hash = hash_bytes(b"before\n");
        let new_hash = hash_bytes(b"after\n");
        let parent = Manifest::new(vec![ManifestEntry {
            path: "old.txt".to_string(),
            blob_hash: old_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let current = Manifest::new(vec![ManifestEntry {
            path: "new.txt".to_string(),
            blob_hash: new_hash.clone(),
            mode: FileMode::Executable,
        }]);
        let changes = vec![FileChange {
            path: "new.txt".to_string(),
            change_type: ChangeType::Renamed,
            old_blob_hash: Some(old_hash),
            new_blob_hash: Some(new_hash),
            old_path: Some("old.txt".to_string()),
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Executable),
        }];

        validate_manifest_transition(&parent, &current, &changes)
            .expect("rename+edit is one valid representation of the exact transition");
    }

    #[test]
    fn semantic_manifest_transition_accepts_equivalent_delete_and_add() {
        let old_hash = hash_bytes(b"before\n");
        let new_hash = hash_bytes(b"after\n");
        let parent = Manifest::new(vec![ManifestEntry {
            path: "old.txt".to_string(),
            blob_hash: old_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let current = Manifest::new(vec![ManifestEntry {
            path: "new.txt".to_string(),
            blob_hash: new_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let changes = vec![
            FileChange {
                path: "old.txt".to_string(),
                change_type: ChangeType::Deleted,
                old_blob_hash: Some(old_hash),
                new_blob_hash: None,
                old_path: None,
                old_mode: Some(FileMode::Regular),
                new_mode: None,
            },
            FileChange {
                path: "new.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(new_hash),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            },
        ];

        validate_manifest_transition(&parent, &current, &changes)
            .expect("delete+add is an equivalent valid representation");
    }

    #[test]
    fn semantic_manifest_transition_accepts_content_and_mode_modification() {
        let old_hash = hash_bytes(b"before\n");
        let new_hash = hash_bytes(b"after\n");
        let parent = Manifest::new(vec![ManifestEntry {
            path: "script".to_string(),
            blob_hash: old_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let current = Manifest::new(vec![ManifestEntry {
            path: "script".to_string(),
            blob_hash: new_hash.clone(),
            mode: FileMode::Executable,
        }]);
        let changes = vec![FileChange {
            path: "script".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(old_hash),
            new_blob_hash: Some(new_hash),
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Executable),
        }];

        validate_manifest_transition(&parent, &current, &changes)
            .expect("content+mode modification describes the exact transition");
    }

    #[test]
    fn semantic_manifest_transition_rejects_forged_duplicate_and_incomplete_records() {
        let old_hash = hash_bytes(b"before\n");
        let new_hash = hash_bytes(b"after\n");
        let parent = Manifest::new(vec![ManifestEntry {
            path: "file".to_string(),
            blob_hash: old_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let current = Manifest::new(vec![ManifestEntry {
            path: "file".to_string(),
            blob_hash: new_hash.clone(),
            mode: FileMode::Executable,
        }]);
        let valid = FileChange {
            path: "file".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(old_hash.clone()),
            new_blob_hash: Some(new_hash),
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Executable),
        };

        let mut forged = valid.clone();
        forged.old_blob_hash = Some(hash_bytes(b"forged"));
        assert!(validate_manifest_transition(&parent, &current, &[forged]).is_err());

        let mut wrong_mode = valid.clone();
        wrong_mode.new_mode = Some(FileMode::Symlink);
        assert!(validate_manifest_transition(&parent, &current, &[wrong_mode]).is_err());

        assert!(validate_manifest_transition(&parent, &current, &[valid.clone(), valid]).is_err());
        assert!(validate_manifest_transition(&parent, &current, &[]).is_err());
    }

    #[test]
    fn semantic_manifest_transition_is_independent_of_overwrite_record_order() {
        let a_hash = hash_bytes(b"a\n");
        let b_hash = hash_bytes(b"b\n");
        let parent = Manifest::new(vec![
            ManifestEntry {
                path: "a".to_string(),
                blob_hash: a_hash.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "b".to_string(),
                blob_hash: b_hash.clone(),
                mode: FileMode::Regular,
            },
        ]);
        let current = Manifest::new(vec![ManifestEntry {
            path: "b".to_string(),
            blob_hash: a_hash.clone(),
            mode: FileMode::Regular,
        }]);
        let rename = FileChange {
            path: "b".to_string(),
            change_type: ChangeType::Renamed,
            old_blob_hash: Some(a_hash),
            new_blob_hash: Some(current.entries[0].blob_hash.clone()),
            old_path: Some("a".to_string()),
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Regular),
        };
        let delete = FileChange {
            path: "b".to_string(),
            change_type: ChangeType::Deleted,
            old_blob_hash: Some(b_hash),
            new_blob_hash: None,
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: None,
        };

        validate_manifest_transition(&parent, &current, &[rename.clone(), delete.clone()])
            .expect("rename before destination deletion is semantically valid");
        validate_manifest_transition(&parent, &current, &[delete, rename])
            .expect("reversing the same records must remain valid");
    }

    #[test]
    fn semantic_manifest_transition_accepts_rename_cycle() {
        let a_hash = hash_bytes(b"a\n");
        let b_hash = hash_bytes(b"b\n");
        let parent = Manifest::new(vec![
            ManifestEntry {
                path: "a".to_string(),
                blob_hash: a_hash.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "b".to_string(),
                blob_hash: b_hash.clone(),
                mode: FileMode::Executable,
            },
        ]);
        let current = Manifest::new(vec![
            ManifestEntry {
                path: "a".to_string(),
                blob_hash: b_hash.clone(),
                mode: FileMode::Executable,
            },
            ManifestEntry {
                path: "b".to_string(),
                blob_hash: a_hash.clone(),
                mode: FileMode::Regular,
            },
        ]);
        let changes = vec![
            FileChange {
                path: "b".to_string(),
                change_type: ChangeType::Renamed,
                old_blob_hash: Some(a_hash.clone()),
                new_blob_hash: Some(a_hash),
                old_path: Some("a".to_string()),
                old_mode: Some(FileMode::Regular),
                new_mode: Some(FileMode::Regular),
            },
            FileChange {
                path: "a".to_string(),
                change_type: ChangeType::Renamed,
                old_blob_hash: Some(b_hash.clone()),
                new_blob_hash: Some(b_hash),
                old_path: Some("b".to_string()),
                old_mode: Some(FileMode::Executable),
                new_mode: Some(FileMode::Executable),
            },
        ];

        validate_manifest_transition(&parent, &current, &changes)
            .expect("a two-path rename cycle is an order-independent transition");
    }

    #[test]
    fn test_diff_with_content_renames_below_threshold_stays_delete_add() {
        let (old, mut blobs) =
            manifest_with_blobs(&[("a.txt", "alpha\nbeta\ngamma\ndelta\nepsilon\n")]);
        let (new, new_blobs) = manifest_with_blobs(&[("b.txt", "one\ntwo\nthree\nfour\nfive\n")]);
        blobs.extend(new_blobs);

        let changes = old
            .diff_with_content_renames(&new, blob_loader(&blobs), &RenameConfig::default())
            .unwrap();

        assert_eq!(changes.len(), 2, "expected D+A, got {changes:?}");
        assert!(changes
            .iter()
            .any(|c| c.path == "a.txt" && c.change_type == ChangeType::Deleted));
        assert!(changes
            .iter()
            .any(|c| c.path == "b.txt" && c.change_type == ChangeType::Added));
    }

    #[test]
    fn test_diff_with_content_renames_respects_size_cap() {
        // Nearly identical contents, but larger than max_file_bytes on both
        // sides — the pass must not pair them (and must not need their bytes).
        let big_old = "shared line\n".repeat(100);
        let big_new = format!("{}{}", "shared line\n".repeat(99), "edited line\n");
        let (old, mut blobs) = manifest_with_blobs(&[("old_big.txt", big_old.as_str())]);
        let (new, new_blobs) = manifest_with_blobs(&[("new_big.txt", big_new.as_str())]);
        blobs.extend(new_blobs);

        let config = RenameConfig {
            max_file_bytes: 64,
            ..RenameConfig::default()
        };
        let changes = old
            .diff_with_content_renames(&new, blob_loader(&blobs), &config)
            .unwrap();

        assert_eq!(changes.len(), 2, "expected D+A, got {changes:?}");
        assert!(!changes.iter().any(|c| c.change_type == ChangeType::Renamed));
    }

    #[test]
    fn test_diff_with_content_renames_respects_max_candidates() {
        let (old, mut blobs) = manifest_with_blobs(&[
            ("old_a.txt", "a1\na2\na3\na4\n"),
            ("old_b.txt", "b1\nb2\nb3\nb4\n"),
        ]);
        let (new, new_blobs) = manifest_with_blobs(&[
            ("new_a.txt", "a1\na2\na3\nedited\n"),
            ("new_b.txt", "b1\nb2\nb3\nedited\n"),
        ]);
        blobs.extend(new_blobs);

        // 2 deletes + 2 adds = 4 candidates > 3 -> the content pass bails out.
        let config = RenameConfig {
            max_candidates: 3,
            ..RenameConfig::default()
        };
        let changes = old
            .diff_with_content_renames(&new, blob_loader(&blobs), &config)
            .unwrap();

        assert_eq!(changes.len(), 4, "expected 2xD + 2xA, got {changes:?}");
        assert!(!changes.iter().any(|c| c.change_type == ChangeType::Renamed));

        // With the cap lifted, the same pairs are promoted to renames.
        let changes = old
            .diff_with_content_renames(&new, blob_loader(&blobs), &RenameConfig::default())
            .unwrap();
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.change_type == ChangeType::Renamed)
                .count(),
            2,
            "expected both pairs renamed, got {changes:?}"
        );
    }
}
