//! Tree-structured manifests.
//!
//! Each Tree contains its DIRECT children (subtrees + blobs), keyed by name.
//! A commit references the root tree's hash; subtrees that don't change between
//! commits are shared by hash reference, giving O(changes × depth) writes
//! instead of O(files) per commit.

use crate::error::{OakError, Result};
use crate::hash::{hash_bytes, hash_string, Hash};
use crate::{FileChange, FileMode, Manifest, ManifestEntry, RenameConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a tree entry refers to: another tree (subdirectory) or a blob (file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeEntryKind {
    Tree,
    Blob,
}

impl TreeEntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TreeEntryKind::Tree => "tree",
            TreeEntryKind::Blob => "blob",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tree" => Some(TreeEntryKind::Tree),
            "blob" => Some(TreeEntryKind::Blob),
            _ => None,
        }
    }
}

/// A single entry in a tree: one direct child (file or subdirectory).
/// `mode` is meaningful only when `kind == Blob`; for trees it is canonicalized
/// to `Regular` and serialized as `"tree"` in the hash recipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: TreeEntryKind,
    pub hash: Hash,
    #[serde(default)]
    pub mode: FileMode,
}

/// A content-addressed tree object. `entries` are sorted by `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub hash: Hash,
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Build a tree from its direct children, sorting and computing the hash.
    /// Rejects entry names that would corrupt the canonical preimage (see
    /// [`validate_entry_name`]).
    pub fn new(entries: Vec<TreeEntry>) -> Result<Self> {
        for e in &entries {
            validate_entry_name(&e.name)?;
        }
        Ok(Self::new_unchecked(entries))
    }

    /// [`Tree::new`] without name validation. Only for callers whose entries
    /// were already validated ([`build_tree`]) or whose result is never
    /// stored ([`build_tree_unchecked`] for `Manifest::new`'s hash-only use).
    fn new_unchecked(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let hash = hash_bytes(&serialize_entries(&entries));
        Tree { hash, entries }
    }

    /// The canonical byte preimage this tree's hash is computed over: one line
    /// per entry (sorted by name), `kind\tname\thash\tmode`, joined by `\n`.
    ///
    /// This is also the on-disk serialized form stored (compressed) in the
    /// `trees.content` column — keeping the stored bytes identical to the hash
    /// recipe means integrity is a single `hash_bytes(content) == hash` check.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serialize_entries(&self.entries)
    }

    /// Reconstruct a tree from its canonical byte form (the inverse of
    /// [`Tree::canonical_bytes`]). `hash` is taken as the storage key; callers
    /// that want integrity should compare `hash_bytes(bytes)` against it.
    ///
    /// Parsing is strict: names must pass [`validate_entry_name`], hashes must
    /// be valid hex, and the mode field must round-trip through
    /// [`mode_string`] exactly. Anything [`serialize_entries`] could not have
    /// produced is rejected rather than silently mis-parsed — defense against
    /// malicious wire or database bytes.
    pub fn from_canonical_bytes(hash: Hash, bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| OakError::Database(format!("tree content not utf-8: {e}")))?;
        let mut entries = Vec::new();
        if !text.is_empty() {
            for line in text.split('\n') {
                let malformed = || OakError::Database(format!("malformed tree entry: {line:?}"));
                let mut parts = line.splitn(4, '\t');
                let kind = parts
                    .next()
                    .and_then(TreeEntryKind::parse)
                    .ok_or_else(malformed)?;
                let name = parts.next().ok_or_else(malformed)?;
                validate_entry_name(name)
                    .map_err(|e| OakError::Database(format!("malformed tree entry: {e}")))?;
                let entry_hash =
                    Hash::from_hex(parts.next().ok_or_else(malformed)?).map_err(|_| malformed())?;
                // The mode field is the final `splitn` segment, so crafted
                // trailing `\t...` bytes land here and fail the exact match.
                let mode = match (kind, parts.next().ok_or_else(malformed)?) {
                    (TreeEntryKind::Tree, "tree") => FileMode::Regular,
                    (TreeEntryKind::Blob, "regular") => FileMode::Regular,
                    (TreeEntryKind::Blob, "executable") => FileMode::Executable,
                    (TreeEntryKind::Blob, "symlink") => FileMode::Symlink,
                    _ => return Err(malformed()),
                };
                entries.push(TreeEntry {
                    name: name.to_string(),
                    kind,
                    hash: entry_hash,
                    mode,
                });
            }
        }
        Ok(Tree { hash, entries })
    }

    /// The hash of an empty tree (no entries). Equal to `hash_string("")`.
    pub fn empty() -> Self {
        Tree {
            hash: hash_string(""),
            entries: Vec::new(),
        }
    }

    /// Hash that represents an empty tree. Useful as a sentinel.
    pub fn empty_hash() -> Hash {
        hash_string("")
    }

    /// Find a direct child by name.
    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

/// Why `name` can't be a tree entry name, or `None` if it's fine.
///
/// The canonical tree preimage ([`serialize_entries`]) uses `\t` as its field
/// separator and `\n` as its line separator, so a name containing either
/// would let distinct trees serialize to identical bytes (hash collision) and
/// stored bytes parse back into different entries (round-trip corruption).
/// `\0`, `/`, empty names, and the reserved `.`/`..` components were never
/// representable as single path components; rejecting `.`/`..` also closes
/// path traversal out of the working-tree root on materialization.
fn entry_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("empty filename")
    } else if name == "." || name == ".." {
        Some("'.' and '..' are reserved names")
    } else if name.contains('\n') {
        Some("newline in filename")
    } else if name.contains('\t') {
        Some("tab in filename")
    } else if name.contains('\0') {
        Some("NUL byte in filename")
    } else if name.contains('/') {
        Some("'/' in filename")
    } else {
        None
    }
}

/// Validate a single tree entry name (one path component). See
/// [`entry_name_problem`] for what's rejected and why.
pub fn validate_entry_name(name: &str) -> Result<()> {
    match entry_name_problem(name) {
        Some(problem) => Err(OakError::InvalidPath(format!(
            "invalid tree entry name {name:?}: {problem}"
        ))),
        None => Ok(()),
    }
}

/// Validate every component of a manifest path (normalized first, so leading/
/// trailing/duplicate slashes and backslashes are fine). Rejects components
/// that can't be tree entry names — including `..`, which would otherwise
/// escape the root when the path is materialized.
pub fn validate_tree_path(path: &str) -> Result<()> {
    for component in normalize_path(path).split('/').filter(|c| !c.is_empty()) {
        if let Some(problem) = entry_name_problem(component) {
            return Err(OakError::InvalidPath(format!(
                "cannot track {path:?}: {problem}"
            )));
        }
    }
    Ok(())
}

fn mode_string(kind: TreeEntryKind, mode: FileMode) -> &'static str {
    match (kind, mode) {
        (TreeEntryKind::Tree, _) => "tree",
        (TreeEntryKind::Blob, FileMode::Regular) => "regular",
        (TreeEntryKind::Blob, FileMode::Executable) => "executable",
        (TreeEntryKind::Blob, FileMode::Symlink) => "symlink",
    }
}

/// Serialize a tree's entries into the canonical byte preimage used both for
/// hashing ([`Tree::new`]) and for on-disk storage ([`Tree::canonical_bytes`]).
/// Entries are expected to already be sorted by name.
fn serialize_entries(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = String::new();
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(e.kind.as_str());
        out.push('\t');
        out.push_str(&e.name);
        out.push('\t');
        out.push_str(e.hash.as_str());
        out.push('\t');
        out.push_str(mode_string(e.kind, e.mode));
    }
    out.into_bytes()
}

/// Output of [`build_tree`]. `root_hash` is the commit-referenced hash;
/// `trees` contains every tree object produced (root + intermediates) in
/// arbitrary order. The caller is responsible for storing them all.
#[derive(Debug)]
pub struct BuiltTree {
    pub root_hash: Hash,
    pub trees: Vec<Tree>,
}

/// Normalize a path string for use as a tree-keyed lookup:
/// - replace backslashes with forward slashes
/// - collapse repeated slashes
/// - strip leading and trailing slashes
pub fn normalize_path(path: &str) -> String {
    let s = path.replace('\\', "/");
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' {
            if prev_slash || out.is_empty() {
                continue;
            }
            prev_slash = true;
            out.push(c);
        } else {
            prev_slash = false;
            out.push(c);
        }
    }
    while out.ends_with('/') {
        out.pop();
    }
    out
}

/// Build a tree from a flat list of (path, blob_hash, mode) entries.
///
/// Paths are normalized (forward slashes, no leading/trailing/duplicate slashes)
/// and grouped by directory recursively. Returns the root tree hash and every
/// intermediate tree object that was produced.
///
/// Every path component must pass [`validate_tree_path`]; a path that would
/// corrupt the canonical preimage or escape the root is rejected before any
/// tree object is built.
pub fn build_tree(entries: &[ManifestEntry]) -> Result<BuiltTree> {
    for e in entries {
        validate_tree_path(&e.path)?;
    }
    Ok(build_tree_unchecked(entries))
}

/// [`build_tree`] without path validation. For `Manifest::new`, which only
/// needs the root hash as a diff identity and never stores the tree objects —
/// it must stay infallible. Storage paths go through [`build_tree`].
pub(crate) fn build_tree_unchecked(entries: &[ManifestEntry]) -> BuiltTree {
    let normalized: Vec<ManifestEntry> = entries
        .iter()
        .map(|e| ManifestEntry {
            path: normalize_path(&e.path),
            blob_hash: e.blob_hash.clone(),
            mode: e.mode,
        })
        .filter(|e| !e.path.is_empty())
        .collect();

    let mut trees = Vec::new();
    let root = build_subtree(normalized, &mut trees);
    let root_hash = root.hash.clone();
    trees.push(root);
    BuiltTree { root_hash, trees }
}

fn build_subtree(entries: Vec<ManifestEntry>, trees: &mut Vec<Tree>) -> Tree {
    let mut groups: BTreeMap<String, Vec<ManifestEntry>> = BTreeMap::new();
    let mut tree_entries: Vec<TreeEntry> = Vec::new();

    for e in entries {
        match e.path.find('/') {
            Some(slash) => {
                let component = e.path[..slash].to_string();
                let rest = e.path[slash + 1..].to_string();
                groups.entry(component).or_default().push(ManifestEntry {
                    path: rest,
                    blob_hash: e.blob_hash,
                    mode: e.mode,
                });
            }
            None => {
                tree_entries.push(TreeEntry {
                    name: e.path,
                    kind: TreeEntryKind::Blob,
                    hash: e.blob_hash,
                    mode: e.mode,
                });
            }
        }
    }

    for (name, sub_entries) in groups {
        let subtree = build_subtree(sub_entries, trees);
        let sub_hash = subtree.hash.clone();
        trees.push(subtree);
        tree_entries.push(TreeEntry {
            name,
            kind: TreeEntryKind::Tree,
            hash: sub_hash,
            mode: FileMode::Regular,
        });
    }

    // Names were validated path-component-wise by `build_tree` (or the caller
    // accepted unchecked hashing via `build_tree_unchecked`).
    Tree::new_unchecked(tree_entries)
}

/// Walk a tree depth-first, producing the equivalent flat list of entries
/// (with full paths). Subtrees are fetched via the provided closure.
///
/// The closure typically reads from a Repository; the closure indirection
/// keeps oak-core dependency-free.
pub fn flatten_tree<F>(root: &Hash, fetch: &mut F) -> Result<Vec<ManifestEntry>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if root == &Tree::empty_hash() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let tree = fetch(root)?;
    flatten_recursive(&tree, "", fetch, &mut out)?;
    Ok(out)
}

fn flatten_recursive<F>(
    tree: &Tree,
    prefix: &str,
    fetch: &mut F,
    out: &mut Vec<ManifestEntry>,
) -> Result<()>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    for entry in &tree.entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", prefix, entry.name)
        };
        match entry.kind {
            TreeEntryKind::Blob => {
                out.push(ManifestEntry {
                    path,
                    blob_hash: entry.hash.clone(),
                    mode: entry.mode,
                });
            }
            TreeEntryKind::Tree => {
                let sub = fetch(&entry.hash)?;
                flatten_recursive(&sub, &path, fetch, out)?;
            }
        }
    }
    Ok(())
}

/// Collect every tree object reachable from `root` (the root tree itself plus
/// all transitively reachable subtrees), keyed by hash.
///
/// Used by push/pull to gather every tree that a commit references for wire
/// transfer. Empty-tree sentinel is excluded — receivers materialize it
/// themselves via `Tree::empty()`.
pub fn collect_tree_objects<F>(root: &Hash, fetch: &mut F) -> Result<Vec<Tree>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if root == &Tree::empty_hash() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Tree> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<Hash> = vec![root.clone()];
    while let Some(h) = stack.pop() {
        if !seen.insert(h.0.clone()) {
            continue;
        }
        let tree = fetch(&h)?;
        for entry in &tree.entries {
            if entry.kind == TreeEntryKind::Tree && !seen.contains(&entry.hash.0) {
                stack.push(entry.hash.clone());
            }
        }
        out.push(tree);
    }
    Ok(out)
}

/// Resolve a slash-separated path within the tree rooted at `root`.
/// Returns the final TreeEntry, or None if any component is missing.
pub fn resolve_path<F>(root: &Hash, path: &str, fetch: &mut F) -> Result<Option<TreeEntry>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let path = normalize_path(path);
    if path.is_empty() {
        return Ok(None);
    }
    let mut current = fetch(root)?;
    let mut iter = path.split('/').peekable();
    while let Some(component) = iter.next() {
        let entry = match current.entries.iter().find(|e| e.name == component) {
            Some(e) => e.clone(),
            None => return Ok(None),
        };
        if iter.peek().is_none() {
            return Ok(Some(entry));
        }
        if entry.kind != TreeEntryKind::Tree {
            return Ok(None);
        }
        current = fetch(&entry.hash)?;
    }
    Ok(None)
}

/// Find the subtree rooted at `path` (a directory). Returns `Some(hash)` if the
/// path exists and is a tree, `None` otherwise. An empty path returns the
/// original `root`.
pub fn subtree_at_path<F>(root: &Hash, path: &str, fetch: &mut F) -> Result<Option<Hash>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let path = normalize_path(path);
    if path.is_empty() {
        return Ok(Some(root.clone()));
    }
    match resolve_path(root, &path, fetch)? {
        Some(entry) if entry.kind == TreeEntryKind::Tree => Ok(Some(entry.hash)),
        _ => Ok(None),
    }
}

/// Diff two trees, returning file-level changes with rename detection.
///
/// Currently flattens both sides and reuses [`Manifest::diff_with_renames`].
/// Hash-equal short-circuit at the subtree level is a future optimization.
pub fn diff_trees<F>(
    a_root: Option<&Hash>,
    b_root: Option<&Hash>,
    fetch: &mut F,
    config: &RenameConfig,
) -> Result<Vec<FileChange>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let a_entries = match a_root {
        Some(h) => flatten_tree(h, fetch)?,
        None => Vec::new(),
    };
    let b_entries = match b_root {
        Some(h) => flatten_tree(h, fetch)?,
        None => Vec::new(),
    };
    let a = Manifest::new(a_entries);
    let b = Manifest::new(b_entries);
    Ok(a.diff_with_renames(&b, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(path: &str, content: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string(content),
            mode: FileMode::Regular,
        }
    }

    fn fetcher(trees: Vec<Tree>) -> impl FnMut(&Hash) -> Result<Tree> {
        let map: HashMap<String, Tree> = trees.into_iter().map(|t| (t.hash.0.clone(), t)).collect();
        move |h: &Hash| {
            map.get(&h.0)
                .cloned()
                .ok_or_else(|| crate::error::OakError::Database(format!("tree not found: {h}")))
        }
    }

    #[test]
    fn empty_tree_hash_matches_sentinel() {
        let t = Tree::empty();
        assert_eq!(t.hash, Tree::empty_hash());
        assert_eq!(t.hash, hash_string(""));
    }

    #[test]
    fn tree_hash_is_deterministic() {
        let entries = vec![
            TreeEntry {
                name: "a".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("a"),
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "b".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("b"),
                mode: FileMode::Regular,
            },
        ];
        let t1 = Tree::new(entries.clone()).unwrap();
        // Reverse order — should produce the same hash because Tree::new sorts.
        let t2 = Tree::new(entries.into_iter().rev().collect()).unwrap();
        assert_eq!(t1.hash, t2.hash);
    }

    #[test]
    fn canonical_bytes_is_hash_preimage() {
        let entries = vec![
            TreeEntry {
                name: "a.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("a"),
                mode: FileMode::Executable,
            },
            TreeEntry {
                name: "sub".to_string(),
                kind: TreeEntryKind::Tree,
                hash: hash_string("sub"),
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "link".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("l"),
                mode: FileMode::Symlink,
            },
        ];
        let t = Tree::new(entries).unwrap();
        // The stored bytes are exactly what the hash is computed over.
        assert_eq!(hash_bytes(&t.canonical_bytes()), t.hash);
    }

    #[test]
    fn canonical_bytes_round_trips() {
        let entries = vec![
            TreeEntry {
                name: "a.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("a"),
                mode: FileMode::Executable,
            },
            TreeEntry {
                name: "sub".to_string(),
                kind: TreeEntryKind::Tree,
                hash: hash_string("sub"),
                mode: FileMode::Regular,
            },
        ];
        let t = Tree::new(entries).unwrap();
        let bytes = t.canonical_bytes();
        let t2 = Tree::from_canonical_bytes(t.hash.clone(), &bytes).unwrap();
        assert_eq!(t2.hash, t.hash);
        // Equal canonical bytes ⇒ identical names/kinds/hashes/modes and order.
        assert_eq!(t2.canonical_bytes(), bytes);
    }

    #[test]
    fn from_canonical_bytes_empty_is_empty_tree() {
        let t = Tree::from_canonical_bytes(Tree::empty_hash(), b"").unwrap();
        assert!(t.entries.is_empty());
    }

    #[test]
    fn build_tree_groups_by_directory() {
        let entries = vec![
            entry("src/lib.rs", "lib"),
            entry("src/bin/main.rs", "main"),
            entry("README.md", "readme"),
        ];
        let built = build_tree(&entries).unwrap();

        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        let paths: Vec<&str> = flat.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"src/bin/main.rs"));
        assert!(paths.contains(&"README.md"));
        assert_eq!(flat.len(), 3);
    }

    #[test]
    fn build_tree_dedup_subtrees() {
        // Same subtree content in two places should produce identical sub-hashes.
        let entries_a = vec![entry("dir/a.txt", "same"), entry("dir/b.txt", "same2")];
        let entries_b = vec![
            entry("other/dir/a.txt", "same"),
            entry("other/dir/b.txt", "same2"),
        ];
        let built_a = build_tree(&entries_a).unwrap();
        let built_b = build_tree(&entries_b).unwrap();

        let dir_hash_a = {
            let mut fetch = fetcher(built_a.trees.clone());
            subtree_at_path(&built_a.root_hash, "dir", &mut fetch).unwrap()
        };
        let dir_hash_b = {
            let mut fetch = fetcher(built_b.trees.clone());
            subtree_at_path(&built_b.root_hash, "other/dir", &mut fetch).unwrap()
        };
        assert!(dir_hash_a.is_some());
        assert_eq!(dir_hash_a, dir_hash_b);
    }

    #[test]
    fn build_tree_normalizes_paths() {
        let entries = vec![
            ManifestEntry {
                path: "src\\lib.rs".to_string(),
                blob_hash: hash_string("lib"),
                mode: FileMode::Regular,
            },
            entry("/leading/slash.txt", "x"),
            entry("double//slash.txt", "y"),
        ];
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        let paths: Vec<&str> = flat.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"leading/slash.txt"));
        assert!(paths.contains(&"double/slash.txt"));
    }

    #[test]
    fn flatten_round_trips_through_manifest_hash() {
        let entries = vec![
            entry("a/b/c.txt", "c"),
            entry("a/d.txt", "d"),
            entry("e.txt", "e"),
        ];
        let original = Manifest::new(entries.clone());
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        let round_tripped = Manifest::new(flat);
        assert_eq!(original.hash, round_tripped.hash);
    }

    #[test]
    fn resolve_path_finds_blob_and_tree() {
        let entries = vec![entry("a/b/c.txt", "c"), entry("a/d.txt", "d")];
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);

        let blob = resolve_path(&built.root_hash, "a/b/c.txt", &mut fetch).unwrap();
        assert!(matches!(
            blob.as_ref().map(|e| e.kind),
            Some(TreeEntryKind::Blob)
        ));

        let dir = resolve_path(&built.root_hash, "a/b", &mut fetch).unwrap();
        assert!(matches!(
            dir.as_ref().map(|e| e.kind),
            Some(TreeEntryKind::Tree)
        ));

        let missing = resolve_path(&built.root_hash, "a/zzz.txt", &mut fetch).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn subtree_at_path_returns_root_for_empty() {
        let entries = vec![entry("x.txt", "x")];
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);
        let sub = subtree_at_path(&built.root_hash, "", &mut fetch).unwrap();
        assert_eq!(sub.as_ref(), Some(&built.root_hash));
    }

    #[test]
    fn diff_trees_matches_manifest_diff() {
        let a_entries = vec![entry("a.txt", "a"), entry("b.txt", "b")];
        let b_entries = vec![entry("a.txt", "a-modified"), entry("c.txt", "c")];

        let a_built = build_tree(&a_entries).unwrap();
        let b_built = build_tree(&b_entries).unwrap();

        let mut all = Vec::new();
        all.extend(a_built.trees.clone());
        all.extend(b_built.trees.clone());
        let mut fetch = fetcher(all);

        let tree_changes = diff_trees(
            Some(&a_built.root_hash),
            Some(&b_built.root_hash),
            &mut fetch,
            &RenameConfig::default(),
        )
        .unwrap();

        let manifest_changes = Manifest::new(a_entries)
            .diff_with_renames(&Manifest::new(b_entries), &RenameConfig::default());

        assert_eq!(tree_changes.len(), manifest_changes.len());
    }

    #[test]
    fn empty_root_returns_empty_flatten() {
        let mut fetch = |_: &Hash| -> Result<Tree> { Ok(Tree::empty()) };
        let flat = flatten_tree(&Tree::empty_hash(), &mut fetch).unwrap();
        assert!(flat.is_empty());
    }

    #[test]
    fn build_tree_skips_empty_paths() {
        let entries = vec![
            ManifestEntry {
                path: "".to_string(),
                blob_hash: hash_string("nothing"),
                mode: FileMode::Regular,
            },
            entry("real.txt", "real"),
        ];
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].path, "real.txt");
    }

    #[test]
    fn normalize_path_idempotent() {
        let s = "a/b/c";
        assert_eq!(normalize_path(s), s);
        assert_eq!(normalize_path("/a//b/"), "a/b");
        assert_eq!(normalize_path("a\\b\\c"), "a/b/c");
    }

    fn blob_named(name: &str) -> TreeEntry {
        TreeEntry {
            name: name.to_string(),
            kind: TreeEntryKind::Blob,
            hash: hash_string("content"),
            mode: FileMode::Regular,
        }
    }

    #[test]
    fn tree_new_rejects_forbidden_names() {
        for name in ["a\nb.txt", "a\tb.txt", "a\0b.txt", "a/b.txt", "", ".", ".."] {
            let err = Tree::new(vec![blob_named(name)]).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidPath(_)),
                "{name:?} should be InvalidPath, got {err:?}"
            );
        }
        // A name that merely *contains* dots is fine.
        assert!(Tree::new(vec![blob_named("..gitignore-ish")]).is_ok());
        assert!(Tree::new(vec![blob_named("a.b")]).is_ok());
    }

    #[test]
    fn build_tree_rejects_forbidden_components() {
        for path in [
            "dir/a\nb.txt",
            "a\tb/file.txt",
            "a\0b",
            "../escape.txt",
            "dir/../escape.txt",
            "dir/..",
            "./self.txt",
            "a/./b.txt",
        ] {
            let err = build_tree(&[entry(path, "x")]).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidPath(_)),
                "{path:?} should be InvalidPath, got {err:?}"
            );
        }
        // Normalization quirks that were always accepted stay accepted.
        assert!(build_tree(&[entry("/leading/slash.txt", "x")]).is_ok());
        assert!(build_tree(&[entry("double//slash.txt", "x")]).is_ok());
    }

    #[test]
    fn validate_tree_path_messages_name_the_file() {
        let err = validate_tree_path("a\nb.txt").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a\\nb.txt"), "got: {msg}");
        assert!(msg.contains("newline"), "got: {msg}");
    }

    #[test]
    fn from_canonical_bytes_rejects_malicious_names() {
        let h = hash_string("x");
        let hex = h.as_str();
        // Forbidden parsed names: '/', '.', '..', empty, NUL.
        for name in ["a/b", ".", "..", "", "a\0b"] {
            let line = format!("blob\t{name}\t{hex}\tregular");
            assert!(
                Tree::from_canonical_bytes(hash_string("k"), line.as_bytes()).is_err(),
                "name {name:?} should be rejected"
            );
        }
    }

    #[test]
    fn from_canonical_bytes_rejects_bad_hashes_and_modes() {
        let h = hash_string("x");
        let hex = h.as_str();
        let cases = [
            // hash position isn't hex / wrong length / uppercase
            "blob\tf.txt\tnot-a-hash\tregular".to_string(),
            format!("blob\tf.txt\t{}\tregular", "A".repeat(64)),
            format!("blob\tf.txt\t{}\tregular", "a".repeat(63)),
            // unknown or kind-mismatched mode
            format!("blob\tf.txt\t{hex}\ttree"),
            format!("tree\td\t{hex}\tregular"),
            format!("blob\tf.txt\t{hex}\tweird"),
            // trailing injected field hides in the final splitn segment
            format!("blob\tf.txt\t{hex}\tregular\textra"),
            // missing mode field entirely
            format!("blob\tf.txt\t{hex}"),
            // unknown kind
            format!("link\tf.txt\t{hex}\tregular"),
        ];
        for bytes in &cases {
            assert!(
                Tree::from_canonical_bytes(hash_string("k"), bytes.as_bytes()).is_err(),
                "should reject {bytes:?}"
            );
        }
        // The exact lines serialize_entries produces still parse.
        for good in [
            format!("blob\tf.txt\t{hex}\tregular"),
            format!("blob\tf.txt\t{hex}\texecutable"),
            format!("blob\tf.txt\t{hex}\tsymlink"),
            format!("tree\td\t{hex}\ttree"),
        ] {
            assert!(
                Tree::from_canonical_bytes(hash_string("k"), good.as_bytes()).is_ok(),
                "should accept {good:?}"
            );
        }
    }
}
