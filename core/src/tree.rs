//! Tree-structured manifests.
//!
//! Each Tree contains its DIRECT children (subtrees + blobs), keyed by name.
//! A commit references the root tree's hash; subtrees that don't change between
//! commits are shared by hash reference, giving O(changes × depth) writes
//! instead of O(files) per commit.

use crate::error::Result;
use crate::hash::{hash_string, Hash};
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
    pub fn new(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let content = entries
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}",
                    e.kind.as_str(),
                    e.name,
                    e.hash,
                    mode_string(e.kind, e.mode),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let hash = hash_string(&content);
        Tree { hash, entries }
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

fn mode_string(kind: TreeEntryKind, mode: FileMode) -> &'static str {
    match (kind, mode) {
        (TreeEntryKind::Tree, _) => "tree",
        (TreeEntryKind::Blob, FileMode::Regular) => "regular",
        (TreeEntryKind::Blob, FileMode::Executable) => "executable",
        (TreeEntryKind::Blob, FileMode::Symlink) => "symlink",
    }
}

/// Output of [`build_tree`]. `root_hash` is the commit-referenced hash;
/// `trees` contains every tree object produced (root + intermediates) in
/// arbitrary order. The caller is responsible for storing them all.
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
pub fn build_tree(entries: &[ManifestEntry]) -> BuiltTree {
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

    Tree::new(tree_entries)
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
        let t1 = Tree::new(entries.clone());
        // Reverse order — should produce the same hash because Tree::new sorts.
        let t2 = Tree::new(entries.into_iter().rev().collect());
        assert_eq!(t1.hash, t2.hash);
    }

    #[test]
    fn build_tree_groups_by_directory() {
        let entries = vec![
            entry("src/lib.rs", "lib"),
            entry("src/bin/main.rs", "main"),
            entry("README.md", "readme"),
        ];
        let built = build_tree(&entries);

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
        let built_a = build_tree(&entries_a);
        let built_b = build_tree(&entries_b);

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
        let built = build_tree(&entries);
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
        let built = build_tree(&entries);
        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        let round_tripped = Manifest::new(flat);
        assert_eq!(original.hash, round_tripped.hash);
    }

    #[test]
    fn resolve_path_finds_blob_and_tree() {
        let entries = vec![entry("a/b/c.txt", "c"), entry("a/d.txt", "d")];
        let built = build_tree(&entries);
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
        let built = build_tree(&entries);
        let mut fetch = fetcher(built.trees);
        let sub = subtree_at_path(&built.root_hash, "", &mut fetch).unwrap();
        assert_eq!(sub.as_ref(), Some(&built.root_hash));
    }

    #[test]
    fn diff_trees_matches_manifest_diff() {
        let a_entries = vec![entry("a.txt", "a"), entry("b.txt", "b")];
        let b_entries = vec![entry("a.txt", "a-modified"), entry("c.txt", "c")];

        let a_built = build_tree(&a_entries);
        let b_built = build_tree(&b_entries);

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
        let built = build_tree(&entries);
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
}
