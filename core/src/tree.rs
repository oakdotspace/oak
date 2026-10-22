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
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        for e in &entries {
            validate_entry_name(&e.name)?;
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(duplicate) = entries
            .windows(2)
            .find(|pair| pair[0].name == pair[1].name)
            .map(|pair| pair[0].name.as_str())
        {
            return Err(OakError::InvalidPath(format!(
                "duplicate tree entry name {duplicate:?}"
            )));
        }
        Ok(Self::new_sorted_unchecked(entries))
    }

    /// [`Tree::new`] without name validation. Only for callers whose entries
    /// were already validated ([`build_tree`]) or whose result is never
    /// stored ([`build_tree_unchecked`] for `Manifest::new`'s hash-only use).
    fn new_unchecked(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new_sorted_unchecked(entries)
    }

    fn new_sorted_unchecked(entries: Vec<TreeEntry>) -> Self {
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
    } else if name.chars().any(char::is_control) {
        Some("control character in filename")
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
    let normalized = normalize_path(path);
    if normalized.is_empty() {
        return Err(OakError::InvalidPath(format!(
            "cannot track {path:?}: path is empty"
        )));
    }
    for component in normalized.split('/').filter(|c| !c.is_empty()) {
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
    validate_tree_path_conflicts(entries)?;
    Ok(build_tree_unchecked(entries))
}

fn validate_tree_path_conflicts(entries: &[ManifestEntry]) -> Result<()> {
    let mut paths: Vec<String> = entries
        .iter()
        .map(|e| normalize_path(&e.path))
        .filter(|p| !p.is_empty())
        .collect();
    paths.sort();

    for pair in paths.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        if prev == next {
            return Err(OakError::InvalidPath(format!(
                "cannot build tree with duplicate path {prev:?}"
            )));
        }
        if next
            .strip_prefix(prev)
            .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Err(OakError::InvalidPath(format!(
                "cannot build tree with both file {prev:?} and child path {next:?}"
            )));
        }
    }

    Ok(())
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

/// `prefix/name`, allocated at exactly its final length. `format!` reserves
/// about twice the prefix length here, and a flattened manifest keeps one of
/// these per file.
pub(crate) fn child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        return name.to_string();
    }
    let mut path = String::with_capacity(prefix.len() + 1 + name.len());
    path.push_str(prefix);
    path.push('/');
    path.push_str(name);
    path
}

/// Append every blob under the tree `root` to `out`, with paths prefixed by
/// `prefix` (the tree's own path; empty for a root).
pub(crate) fn flatten_subtree_into<F>(
    root: &Hash,
    prefix: &str,
    fetch: &mut F,
    out: &mut Vec<ManifestEntry>,
) -> Result<()>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if root == &Tree::empty_hash() {
        return Ok(());
    }
    let tree = fetch(root)?;
    flatten_recursive(&tree, prefix, fetch, out)
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
        let path = child_path(prefix, &entry.name);
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
    if root == &Tree::empty_hash() {
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

/// Diff two trees, returning file-level changes with exact-hash rename
/// detection.
///
/// Walks both trees together and descends only into subtrees whose hashes
/// differ, so a change touching `k` paths fetches O(k × depth) trees instead
/// of flattening both sides. The changed entries are then classified by
/// [`Manifest::diff_entries_with_renames`], so the result — rename pairing
/// included — is exactly what flattening both trees in name order and
/// diffing those manifests produces.
pub fn diff_trees<F>(
    a_root: Option<&Hash>,
    b_root: Option<&Hash>,
    fetch: &mut F,
    _config: &RenameConfig,
) -> Result<Vec<FileChange>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let (old_side, new_side) = diff_tree_entries(a_root, b_root, fetch)?;
    let old = Manifest {
        hash: a_root.cloned().unwrap_or_else(Tree::empty_hash),
        entries: old_side,
    };
    Ok(old.diff_entries_with_renames(&new_side))
}

/// The files that differ between trees `a_root` and `b_root`, unpaired: the
/// first list holds each differing path's `a`-side entry, the second its
/// `b`-side entry, so a path present on only one side appears in only one
/// list and a content or mode change appears in both. Walks only subtrees
/// whose hashes differ, like [`diff_trees`].
pub fn diff_tree_entries<F>(
    a_root: Option<&Hash>,
    b_root: Option<&Hash>,
    fetch: &mut F,
) -> Result<(Vec<ManifestEntry>, Vec<ManifestEntry>)>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let empty = Tree::empty_hash();
    let mut old_side = Vec::new();
    let mut new_side = Vec::new();
    diff_subtrees(
        a_root.unwrap_or(&empty),
        b_root.unwrap_or(&empty),
        "",
        fetch,
        &mut old_side,
        &mut new_side,
    )?;
    Ok((old_side, new_side))
}

/// Apply file-level edits to the tree at `root` without flattening it: each
/// path in `removals` drops the file there (a path that holds no file is left
/// alone), then each entry in `upserts` adds or replaces the file at its path.
/// Only the directories on edited paths are fetched and rebuilt —
/// O(edits × depth) instead of O(files).
///
/// The root equals [`build_tree`] over the flattened `root` with the same
/// edits applied, and fails where that would: a file and a child path at one
/// path, or two upserts of one path, are an [`OakError::InvalidPath`] error.
/// `trees` holds only the directories the edits created; unchanged subtrees
/// are referenced by hash.
pub fn edit_tree<F>(
    root: &Hash,
    upserts: &[ManifestEntry],
    removals: &[String],
    fetch: &mut F,
) -> Result<BuiltTree>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if upserts.is_empty() && removals.is_empty() {
        return Ok(BuiltTree {
            root_hash: root.clone(),
            trees: Vec::new(),
        });
    }
    let mut edits: Vec<(String, Option<&ManifestEntry>)> = Vec::new();
    for path in removals {
        let path = normalize_path(path);
        if !path.is_empty() {
            edits.push((path, None));
        }
    }
    for entry in upserts {
        validate_tree_path(&entry.path)?;
        edits.push((normalize_path(&entry.path), Some(entry)));
    }
    let edits: Vec<(&str, Option<&ManifestEntry>)> = edits
        .iter()
        .map(|(path, edit)| (path.as_str(), *edit))
        .collect();
    let mut trees = Vec::new();
    let root_hash =
        edit_dir(dir_hash(root), edits, "", fetch, &mut trees)?.unwrap_or_else(Tree::empty_hash);
    Ok(BuiltTree { root_hash, trees })
}

/// A directory hash, with the empty tree normalized to "no directory".
pub(crate) fn dir_hash(hash: &Hash) -> Option<&Hash> {
    (hash != &Tree::empty_hash()).then_some(hash)
}

/// Apply `edits` (paths relative to this directory; `None` removes) to the
/// directory `dir` at `prefix`. Returns the new directory's hash, or `None`
/// when it ends up empty.
fn edit_dir<F>(
    dir: Option<&Hash>,
    edits: Vec<(&str, Option<&ManifestEntry>)>,
    prefix: &str,
    fetch: &mut F,
    trees: &mut Vec<Tree>,
) -> Result<Option<Hash>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let tree = match dir {
        Some(hash) => load_tree(hash, fetch)?,
        None => Tree::empty(),
    };
    let mut entries: BTreeMap<String, TreeEntry> = tree
        .entries
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect();
    let mut here: BTreeMap<&str, Vec<Option<&ManifestEntry>>> = BTreeMap::new();
    let mut below: BTreeMap<&str, Vec<(&str, Option<&ManifestEntry>)>> = BTreeMap::new();
    for (path, edit) in edits {
        match path.split_once('/') {
            None => here.entry(path).or_default().push(edit),
            Some((name, rest)) => below.entry(name).or_default().push((rest, edit)),
        }
    }
    let mut names: Vec<&str> = here.keys().chain(below.keys()).copied().collect();
    names.sort_unstable();
    names.dedup();

    for name in names {
        let path = child_path(prefix, name);
        let direct = here.remove(name).unwrap_or_default();
        let puts: Vec<&ManifestEntry> = direct.iter().flatten().copied().collect();
        if puts.len() > 1 {
            return Err(OakError::InvalidPath(format!(
                "cannot build tree with duplicate path {path:?}"
            )));
        }
        let mut node = entries.remove(name);
        if direct.iter().any(Option::is_none)
            && node.as_ref().is_some_and(|e| e.kind == TreeEntryKind::Blob)
        {
            node = None;
        }
        if let Some(nested) = below.remove(name) {
            match node.as_ref() {
                // Removing files beneath a file is a no-op; adding one clashes.
                Some(file) if file.kind == TreeEntryKind::Blob => {
                    if let Some(child) = nested.iter().find_map(|(_, edit)| *edit) {
                        return Err(OakError::InvalidPath(format!(
                            "cannot build tree with both file {path:?} and child path {:?}",
                            normalize_path(&child.path)
                        )));
                    }
                }
                _ => {
                    let sub = node.as_ref().map(|e| e.hash.clone());
                    node = edit_dir(sub.as_ref().and_then(dir_hash), nested, &path, fetch, trees)?
                        .map(|hash| TreeEntry {
                            name: name.to_string(),
                            kind: TreeEntryKind::Tree,
                            hash,
                            mode: FileMode::Regular,
                        });
                }
            }
        }
        if let Some(file) = puts.first() {
            if let Some(dir) = node
                .as_ref()
                .filter(|e| e.kind == TreeEntryKind::Tree && e.hash != Tree::empty_hash())
            {
                let child = first_file_under(&dir.hash, &path, trees, fetch)?;
                return Err(OakError::InvalidPath(format!(
                    "cannot build tree with both file {path:?} and child path {child:?}"
                )));
            }
            node = Some(TreeEntry {
                name: name.to_string(),
                kind: TreeEntryKind::Blob,
                hash: file.blob_hash.clone(),
                mode: file.mode,
            });
        }
        if let Some(node) = node {
            entries.insert(name.to_string(), node);
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }
    let tree = Tree::new(entries.into_values().collect())?;
    let hash = tree.hash.clone();
    if Some(&hash) != dir {
        trees.push(tree);
    }
    Ok(Some(hash))
}

/// Some file path beneath the directory `dir` at `prefix`, for error messages.
/// `created` holds directories the edit built and hasn't stored.
fn first_file_under<F>(dir: &Hash, prefix: &str, created: &[Tree], fetch: &mut F) -> Result<String>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let mut load = |hash: &Hash| match created.iter().find(|t| &t.hash == hash) {
        Some(tree) => Ok(tree.clone()),
        None => load_tree(hash, fetch),
    };
    let mut path = prefix.to_string();
    let mut current = load(dir)?;
    while let Some(entry) = entries_by_name(&current).first().copied().cloned() {
        path = child_path(&path, &entry.name);
        if entry.kind == TreeEntryKind::Blob {
            break;
        }
        current = load(&entry.hash)?;
    }
    Ok(path)
}

/// Fetch a tree, answering the empty-tree sentinel without a lookup (it is
/// never stored).
pub(crate) fn load_tree<F>(hash: &Hash, fetch: &mut F) -> Result<Tree>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if hash == &Tree::empty_hash() {
        Ok(Tree::empty())
    } else {
        fetch(hash)
    }
}

/// A tree's entries in name order. Oak trees are stored sorted, but other
/// backends (git sorts directories as `name/`) need not be.
pub(crate) fn entries_by_name(tree: &Tree) -> Vec<&TreeEntry> {
    let mut entries: Vec<&TreeEntry> = tree.entries.iter().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Collect the entries that differ between trees `a` and `b` (both rooted at
/// `prefix`): `old` gets the `a`-side files, `new` the `b`-side files.
/// Subtrees with equal hashes are skipped without being fetched.
fn diff_subtrees<F>(
    a: &Hash,
    b: &Hash,
    prefix: &str,
    fetch: &mut F,
    old: &mut Vec<ManifestEntry>,
    new: &mut Vec<ManifestEntry>,
) -> Result<()>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    if a == b {
        return Ok(());
    }
    let a_tree = load_tree(a, fetch)?;
    let b_tree = load_tree(b, fetch)?;
    let a_entries = entries_by_name(&a_tree);
    let b_entries = entries_by_name(&b_tree);
    let (mut i, mut j) = (0, 0);
    while i < a_entries.len() || j < b_entries.len() {
        let order = match (a_entries.get(i), b_entries.get(j)) {
            (Some(x), Some(y)) => x.name.cmp(&y.name),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, _) => std::cmp::Ordering::Greater,
        };
        match order {
            std::cmp::Ordering::Less => {
                push_entry_files(a_entries[i], prefix, fetch, old)?;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                push_entry_files(b_entries[j], prefix, fetch, new)?;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (x, y) = (a_entries[i], b_entries[j]);
                i += 1;
                j += 1;
                if x.kind == y.kind && x.hash == y.hash && x.mode == y.mode {
                    continue;
                }
                if x.kind == TreeEntryKind::Tree && y.kind == TreeEntryKind::Tree {
                    let path = child_path(prefix, &x.name);
                    diff_subtrees(&x.hash, &y.hash, &path, fetch, old, new)?;
                } else {
                    push_entry_files(x, prefix, fetch, old)?;
                    push_entry_files(y, prefix, fetch, new)?;
                }
            }
        }
    }
    Ok(())
}

/// Every file at or under `entry` (a direct child of the tree at `prefix`).
fn push_entry_files<F>(
    entry: &TreeEntry,
    prefix: &str,
    fetch: &mut F,
    out: &mut Vec<ManifestEntry>,
) -> Result<()>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let path = child_path(prefix, &entry.name);
    match entry.kind {
        TreeEntryKind::Blob => {
            out.push(ManifestEntry {
                path,
                blob_hash: entry.hash.clone(),
                mode: entry.mode,
            });
            Ok(())
        }
        TreeEntryKind::Tree => flatten_subtree_into(&entry.hash, &path, fetch, out),
    }
}

/// Random small repos for checking tree-level walks against their flat
/// equivalents.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;

    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            // xorshift64*
            let mut x = self.0 | 1;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        }
    }

    // A tiny alphabet and shallow depth make sides collide on names and on
    // file-vs-directory positions.
    const NAMES: [&str; 4] = ["a", "b", "c", "d"];

    fn random_path(rng: &mut Rng) -> String {
        let depth = 1 + rng.below(3);
        (0..depth)
            .map(|_| NAMES[rng.below(NAMES.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join("/")
    }

    // Few distinct contents, so exact-hash renames and duplicate blobs occur.
    fn random_blob(rng: &mut Rng) -> Hash {
        hash_string(&format!("content-{}", rng.below(6)))
    }

    fn random_mode(rng: &mut Rng) -> FileMode {
        match rng.below(8) {
            0 => FileMode::Executable,
            1 => FileMode::Symlink,
            _ => FileMode::Regular,
        }
    }

    fn is_under(path: &str, dir: &str) -> bool {
        path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
    }

    /// Add `entry`, dropping whatever it would clash with: the same path, a
    /// file at one of its ancestor directories, or files beneath it.
    fn put(entries: &mut Vec<ManifestEntry>, entry: ManifestEntry) {
        entries.retain(|e| {
            e.path != entry.path
                && !is_under(&entry.path, &e.path)
                && !is_under(&e.path, &entry.path)
        });
        entries.push(entry);
    }

    pub(crate) fn random_entries(rng: &mut Rng, files: u64) -> Vec<ManifestEntry> {
        let mut entries = Vec::new();
        for _ in 0..files {
            let entry = ManifestEntry {
                path: random_path(rng),
                blob_hash: random_blob(rng),
                mode: random_mode(rng),
            };
            put(&mut entries, entry);
        }
        entries
    }

    /// Apply `edits` random edits: content changes, mode flips, deletes,
    /// adds, renames, and file/directory replacements.
    pub(crate) fn mutate(rng: &mut Rng, base: &[ManifestEntry], edits: u64) -> Vec<ManifestEntry> {
        let mut entries = base.to_vec();
        for _ in 0..edits {
            let pick = |rng: &mut Rng, entries: &[ManifestEntry]| {
                (!entries.is_empty()).then(|| rng.below(entries.len() as u64) as usize)
            };
            match rng.below(6) {
                0 => {
                    if let Some(i) = pick(rng, &entries) {
                        entries[i].blob_hash = random_blob(rng);
                    }
                }
                1 => {
                    if let Some(i) = pick(rng, &entries) {
                        entries[i].mode = random_mode(rng);
                    }
                }
                2 => {
                    if let Some(i) = pick(rng, &entries) {
                        entries.remove(i);
                    }
                }
                3 => {
                    if let Some(i) = pick(rng, &entries) {
                        let moved = entries.remove(i);
                        let path = random_path(rng);
                        put(&mut entries, ManifestEntry { path, ..moved });
                    }
                }
                4 => {
                    // Replace a directory with a file (or a file with a dir).
                    if let Some(i) = pick(rng, &entries) {
                        let path = entries[i].path.clone();
                        let target = match rng.below(2) {
                            0 => path.rsplit_once('/').map(|(dir, _)| dir.to_string()),
                            _ => Some(format!("{path}/{}", NAMES[rng.below(4) as usize])),
                        };
                        if let Some(path) = target {
                            let entry = ManifestEntry {
                                path,
                                blob_hash: random_blob(rng),
                                mode: random_mode(rng),
                            };
                            put(&mut entries, entry);
                        }
                    }
                }
                _ => {
                    let entry = ManifestEntry {
                        path: random_path(rng),
                        blob_hash: random_blob(rng),
                        mode: random_mode(rng),
                    };
                    put(&mut entries, entry);
                }
            }
        }
        entries
    }

    /// An in-memory tree store.
    #[derive(Default)]
    pub(crate) struct Store {
        pub(crate) trees: HashMap<Hash, Tree>,
    }

    impl Store {
        pub(crate) fn add_entries(&mut self, entries: &[ManifestEntry]) -> Hash {
            let built = build_tree(entries).expect("random entries are clash-free");
            for tree in built.trees {
                self.trees.insert(tree.hash.clone(), tree);
            }
            built.root_hash
        }

        pub(crate) fn add_trees(&mut self, trees: Vec<Tree>) {
            for tree in trees {
                self.trees.insert(tree.hash.clone(), tree);
            }
        }

        pub(crate) fn fetch(&self, hash: &Hash) -> Result<Tree> {
            self.trees
                .get(hash)
                .cloned()
                .ok_or_else(|| OakError::Database(format!("tree not found: {hash}")))
        }
    }
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
    fn build_tree_rejects_file_directory_collision() {
        let entries = vec![entry("src", "file"), entry("src/main.rs", "main")];
        let err = build_tree(&entries).unwrap_err();
        assert!(
            err.to_string().contains("both file"),
            "expected file/directory collision error, got {err}"
        );
    }

    #[test]
    fn build_tree_rejects_duplicate_paths() {
        let entries = vec![entry("src/main.rs", "one"), entry("src/main.rs", "two")];
        let err = build_tree(&entries).unwrap_err();
        assert!(
            err.to_string().contains("duplicate path"),
            "expected duplicate path error, got {err}"
        );
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
    fn resolve_path_empty_tree_returns_missing_without_fetch() {
        let mut fetched = false;
        let mut fetch = |_h: &Hash| {
            fetched = true;
            Err(crate::error::OakError::Database(
                "empty tree should not be fetched".to_string(),
            ))
        };

        let missing = resolve_path(&Tree::empty_hash(), "anything.txt", &mut fetch).unwrap();
        assert!(missing.is_none());
        assert!(!fetched);
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

    fn change_keys(changes: &[FileChange]) -> Vec<String> {
        changes.iter().map(|c| format!("{c:?}")).collect()
    }

    #[test]
    fn diff_trees_matches_flat_diff_on_random_trees() {
        use super::test_support::{mutate, random_entries, Rng, Store};
        for seed in 0..2000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let files = 1 + rng.below(12);
            let base = random_entries(&mut rng, files);
            let edits = rng.below(5);
            let other = mutate(&mut rng, &base, edits);
            let mut store = Store::default();
            let a = store.add_entries(&base);
            let b = store.add_entries(&other);
            let mut fetch = |h: &Hash| store.fetch(h);

            let walked =
                diff_trees(Some(&a), Some(&b), &mut fetch, &RenameConfig::default()).unwrap();
            let a_flat = flatten_tree(&a, &mut fetch).unwrap();
            let b_flat = flatten_tree(&b, &mut fetch).unwrap();
            let flat = Manifest {
                hash: a.clone(),
                entries: a_flat,
            }
            .diff_entries_with_renames(&b_flat);
            assert_eq!(change_keys(&walked), change_keys(&flat), "seed {seed}");

            let from_nothing = diff_trees(None, Some(&b), &mut fetch, &RenameConfig::default());
            assert_eq!(from_nothing.unwrap().len(), b_flat.len(), "seed {seed}");
        }
    }

    #[test]
    fn edit_tree_matches_flat_rebuild_on_random_trees() {
        use super::test_support::{mutate, random_entries, Rng, Store};
        let (mut built, mut rejected) = (0, 0);
        for seed in 0..2000u64 {
            let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) ^ 0xed17);
            let files = 1 + rng.below(12);
            let base = random_entries(&mut rng, files);
            let mut store = Store::default();
            let root = store.add_entries(&base);
            // Upserts from a random mutation (some clash with what stays),
            // removals from base paths plus a few that hold no file.
            let edits = rng.below(5);
            let upserts: Vec<ManifestEntry> = mutate(&mut rng, &base, edits)
                .into_iter()
                .filter(|_| rng.below(3) == 0)
                .collect();
            let mut removals: Vec<String> = base
                .iter()
                .filter(|_| rng.below(4) == 0)
                .map(|e| e.path.clone())
                .collect();
            if rng.below(2) == 0 {
                removals.push(["a", "b/c", "d/a/b"][rng.below(3) as usize].to_string());
            }

            let mut flat: Vec<ManifestEntry> = base
                .iter()
                .filter(|e| !removals.contains(&e.path))
                .cloned()
                .collect();
            flat.retain(|e| !upserts.iter().any(|u| u.path == e.path));
            flat.extend(upserts.iter().cloned());
            let expected = build_tree(&flat);
            let edited = {
                let mut fetch = |h: &Hash| store.fetch(h);
                edit_tree(&root, &upserts, &removals, &mut fetch)
            };
            match (expected, edited) {
                (Ok(expected), Ok(edited)) => {
                    assert_eq!(edited.root_hash, expected.root_hash, "seed {seed}");
                    store.add_trees(edited.trees);
                    let mut fetch = |h: &Hash| store.fetch(h);
                    let walked = flatten_tree(&edited.root_hash, &mut fetch).unwrap();
                    assert_eq!(walked.len(), flat.len(), "seed {seed}");
                    built += 1;
                }
                (Err(_), Err(OakError::InvalidPath(_))) => rejected += 1,
                (expected, edited) => panic!(
                    "seed {seed}: flat build {:?} vs edit {:?}",
                    expected.map(|b| b.root_hash),
                    edited.map(|b| b.root_hash)
                ),
            }
        }
        assert!(built > 1000, "{built} built");
        assert!(rejected > 0, "no file/directory clash cases generated");
    }

    #[test]
    fn edit_tree_rebuilds_only_the_edited_directories() {
        let mut entries = Vec::new();
        for d in 0..8 {
            for f in 0..8 {
                entries.push(entry(&format!("d{d}/f{f}.txt"), &format!("{d}{f}")));
            }
        }
        let base = build_tree(&entries).unwrap();
        let mut inner = fetcher(base.trees);
        let mut fetches = 0;
        let mut fetch = |h: &Hash| {
            fetches += 1;
            inner(h)
        };
        let edited = edit_tree(
            &base.root_hash,
            &[entry("d3/f1.txt", "edited"), entry("new/file.txt", "new")],
            &["d5/f0.txt".to_string(), "d9/missing.txt".to_string()],
            &mut fetch,
        )
        .unwrap();
        // The root plus d3 and d5; `new/` and `d9/` don't exist yet.
        assert_eq!(fetches, 3);
        // New root, d3, d5 and new/.
        assert_eq!(edited.trees.len(), 4);
        entries[25].blob_hash = hash_string("edited");
        entries.remove(40);
        entries.push(entry("new/file.txt", "new"));
        assert_eq!(edited.root_hash, build_tree(&entries).unwrap().root_hash);

        let mut no_fetch = |h: &Hash| -> Result<Tree> { panic!("unexpected fetch of {h}") };
        let unchanged = edit_tree(&edited.root_hash, &[], &[], &mut no_fetch).unwrap();
        assert_eq!(unchanged.root_hash, edited.root_hash);
        assert!(unchanged.trees.is_empty());
    }

    #[test]
    fn diff_trees_fetches_only_the_changed_path() {
        // 8 top-level dirs × 8 subdirs × 8 files; change one file.
        let mut entries = Vec::new();
        for d in 0..8 {
            for s in 0..8 {
                for f in 0..8 {
                    entries.push(entry(&format!("d{d}/s{s}/f{f}.txt"), &format!("{d}{s}{f}")));
                }
            }
        }
        let a = build_tree(&entries).unwrap();
        entries[200].blob_hash = hash_string("changed");
        let changed_path = entries[200].path.clone();
        let b = build_tree(&entries).unwrap();
        let mut all = a.trees;
        all.extend(b.trees);
        let mut inner = fetcher(all);
        let mut fetches = 0;
        let mut fetch = |h: &Hash| {
            fetches += 1;
            inner(h)
        };
        let changes = diff_trees(
            Some(&a.root_hash),
            Some(&b.root_hash),
            &mut fetch,
            &RenameConfig::default(),
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, changed_path);
        assert_eq!(changes[0].change_type, crate::ChangeType::Modified);
        // root, d*, s* on each side.
        assert_eq!(fetches, 6);
    }

    #[test]
    fn child_paths_are_allocated_exactly() {
        let entries = vec![entry("some/long/directory/name/file.txt", "x")];
        let built = build_tree(&entries).unwrap();
        let mut fetch = fetcher(built.trees);
        let flat = flatten_tree(&built.root_hash, &mut fetch).unwrap();
        assert_eq!(flat[0].path.capacity(), flat[0].path.len());
    }

    #[test]
    fn empty_root_returns_empty_flatten() {
        let mut fetch = |_: &Hash| -> Result<Tree> { Ok(Tree::empty()) };
        let flat = flatten_tree(&Tree::empty_hash(), &mut fetch).unwrap();
        assert!(flat.is_empty());
    }

    #[test]
    fn build_tree_rejects_empty_paths_without_silently_dropping_entries() {
        let entries = vec![
            ManifestEntry {
                path: "".to_string(),
                blob_hash: hash_string("nothing"),
                mode: FileMode::Regular,
            },
            entry("real.txt", "real"),
        ];

        let err = build_tree(&entries).unwrap_err();

        assert!(
            err.to_string().contains("path is empty"),
            "unexpected error: {err}"
        );
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
        for name in [
            "a\nb.txt",
            "a\tb.txt",
            "a\0b.txt",
            "a\u{1b}b.txt",
            "a\u{7f}b.txt",
            "a/b.txt",
            "",
            ".",
            "..",
        ] {
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
    fn tree_new_rejects_duplicate_names() {
        let mut first = blob_named("same.txt");
        first.hash = hash_string("first");
        let mut second = blob_named("same.txt");
        second.hash = hash_string("second");

        let err = Tree::new(vec![first, second]).unwrap_err();
        assert!(
            err.to_string().contains("duplicate tree entry name"),
            "expected duplicate entry error, got {err}"
        );
    }

    #[test]
    fn build_tree_rejects_forbidden_components() {
        for path in [
            "",
            "/",
            "///",
            "dir/a\nb.txt",
            "a\tb/file.txt",
            "a\0b",
            "a\u{1b}b",
            "a\u{7f}b",
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
