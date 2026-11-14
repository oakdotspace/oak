//! Views — pruned, hash-verifiable slices of a repository.
//!
//! A **view** is a set of path prefixes to include, minus prefixes to exclude,
//! plus a few *spine file* names (`AGENTS.md`, `CLAUDE.md`) kept in every
//! directory on the way down to an included prefix. It is what a partial
//! clone or mount (`--path DIR`), or a reader who can't see a restricted
//! subtree, receives.
//!
//! Unlike a [`crate::SparseCone`], which withholds blob *content* but ships
//! every tree, a view **prunes the trees themselves**: out-of-view names are
//! absent, not just empty. That's possible without weakening hash
//! verification because the pruned tree is served under its own commit
//! (a *view commit*, see [`derive_view_commit`]) whose `manifest_hash` is the
//! pruned root, so the client verifies it like any other commit.
//!
//! Pruning is cheap: every directory wholly inside the view keeps its
//! existing hash, so only the spine directories between the root and each
//! included prefix are rebuilt — O(depth × prefixes), not O(files).
//! [`graft_tree`] is the inverse used on push: it splices a view's in-view
//! content back into the full tree and refuses anything outside the view.

use crate::error::{OakError, Result};
use crate::hash::{hash_bytes, Hash};
use crate::sparse::normalize_prefix;
use crate::tree::{child_path, dir_hash, load_tree, validate_entry_name, validate_tree_path};
use crate::tree::{BuiltTree, Tree, TreeEntry, TreeEntryKind};
use crate::{Commit, FileChange};
use std::collections::{BTreeMap, HashSet};

/// Wire version of view requests (`view_protocol=view_v1`). A server that
/// doesn't echo it back in `effective_view` doesn't understand views, and the
/// client falls back to the full-tree protocol.
pub const VIEW_PROTOCOL: &str = "view_v1";

/// Spine files every view carries by default.
pub const DEFAULT_SPINE_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// The message a view commit carries when its source commit also changed
/// paths the viewer can't read, so the real message could describe them.
pub const REDACTED_VIEW_MESSAGE: &str = "(also changes paths outside your view)";

/// Does `prefix` cover `path`? The empty prefix (the repo root) covers
/// everything; otherwise `path` must equal `prefix` or sit beneath it on a
/// `/` boundary. Both arguments must already be normalized.
fn prefix_covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

/// How much of a directory a view keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirState {
    /// Nothing at or below the directory is in view.
    Outside,
    /// The whole directory is in view; its hash is reused as-is.
    Full,
    /// Some of it is: walk the entries.
    Partial,
}

/// A normalized view: included prefixes, excluded prefixes, and spine file
/// names. Construct with [`ViewSpec::new`]; two specs selecting the same
/// paths normalize to the same value and the same [`ViewSpec::id`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewSpec {
    /// Sorted, none covered by another include or by an exclude. `""` is the
    /// whole repo.
    include: Vec<String>,
    /// Sorted, each beneath some include, none covered by another exclude.
    exclude: Vec<String>,
    /// Sorted, deduplicated basenames.
    spine_files: Vec<String>,
}

impl ViewSpec {
    /// Build a normalized view. Prefixes are normalized like sparse-cone
    /// prefixes (`/apps/web/` → `apps/web`); `""` (or `/`) includes the whole
    /// repo. Redundant entries are dropped: an include covered by another
    /// include or by an exclude, and an exclude that no include reaches or
    /// that another exclude covers.
    ///
    /// Errors on an invalid path, an exclude of the whole repo, a spine name
    /// that isn't a single path component, or a view left including nothing.
    pub fn new<I, E, S>(include: I, exclude: E, spine_files: S) -> Result<ViewSpec>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        E: IntoIterator,
        E::Item: AsRef<str>,
        S: IntoIterator,
        S::Item: AsRef<str>,
    {
        let include = normalize_all(include)?;
        let mut exclude = normalize_all(exclude)?;
        if exclude.iter().any(|e| e.is_empty()) {
            return Err(OakError::InvalidPath(
                "a view can't exclude the whole repository".to_string(),
            ));
        }
        exclude = drop_covered(exclude);
        let include: Vec<String> = drop_covered(
            include
                .into_iter()
                .filter(|i| !exclude.iter().any(|e| prefix_covers(e, i)))
                .collect(),
        );
        if include.is_empty() {
            return Err(OakError::InvalidPath(
                "a view must include at least one path".to_string(),
            ));
        }
        exclude.retain(|e| include.iter().any(|i| prefix_covers(i, e)));

        let mut spine: Vec<String> = Vec::new();
        for name in spine_files {
            let name = name.as_ref().trim();
            validate_entry_name(name)?;
            spine.push(name.to_string());
        }
        spine.sort();
        spine.dedup();

        Ok(ViewSpec {
            include,
            exclude,
            spine_files: spine,
        })
    }

    /// The whole repository, with no exclusions or spine files.
    pub fn full() -> ViewSpec {
        ViewSpec {
            include: vec![String::new()],
            exclude: Vec::new(),
            spine_files: Vec::new(),
        }
    }

    pub fn include(&self) -> &[String] {
        &self.include
    }

    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    pub fn spine_files(&self) -> &[String] {
        &self.spine_files
    }

    /// True when the view selects the whole repository.
    pub fn is_full(&self) -> bool {
        self.include == [""] && self.exclude.is_empty()
    }

    /// A new view with this one's includes and spine files plus `other`'s,
    /// keeping an exclude only where both views excluded it or `other`
    /// doesn't reach it — the union of what the two views select.
    pub fn union(&self, other: &ViewSpec) -> Result<ViewSpec> {
        let exclude = self
            .exclude
            .iter()
            .filter(|e| !other.covers(e))
            .chain(other.exclude.iter().filter(|e| !self.covers(e)));
        ViewSpec::new(
            self.include.iter().chain(&other.include),
            exclude,
            self.spine_files.iter().chain(&other.spine_files),
        )
    }

    /// The canonical text form: one line per entry, `+prefix` for includes,
    /// `-prefix` for excludes, `=name` for spine files, in that order. A bare
    /// `+` includes the whole repo. Round-trips with [`ViewSpec::parse`] and
    /// is what [`ViewSpec::id`] hashes, so it's also the wire form.
    pub fn to_canonical(&self) -> String {
        let mut lines = Vec::new();
        lines.extend(self.include.iter().map(|p| format!("+{p}")));
        lines.extend(self.exclude.iter().map(|p| format!("-{p}")));
        lines.extend(self.spine_files.iter().map(|n| format!("={n}")));
        lines.join("\n")
    }

    /// Parse [`ViewSpec::to_canonical`] output (or any hand-written text in
    /// that shape; blank lines are ignored and the result is normalized).
    pub fn parse(text: &str) -> Result<ViewSpec> {
        let (mut include, mut exclude, mut spine) = (Vec::new(), Vec::new(), Vec::new());
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (sigil, rest) = line.split_at(1);
            match sigil {
                "+" => include.push(rest),
                "-" => exclude.push(rest),
                "=" => spine.push(rest),
                _ => {
                    return Err(OakError::InvalidPath(format!(
                        "view line {line:?} must start with +, - or ="
                    )))
                }
            }
        }
        ViewSpec::new(include, exclude, spine)
    }

    /// A stable identifier for this view: BLAKE3 over a version tag and the
    /// canonical form.
    pub fn id(&self) -> Hash {
        hash_bytes(format!("oak-view-v1\n{}", self.to_canonical()).as_bytes())
    }

    fn is_excluded(&self, path: &str) -> bool {
        self.exclude.iter().any(|e| prefix_covers(e, path))
    }

    /// Is `path` inside an included prefix and not excluded? Spine files are
    /// not counted; see [`ViewSpec::includes_file`].
    pub fn covers(&self, path: &str) -> bool {
        let path = normalize_prefix(path);
        self.include.iter().any(|i| prefix_covers(i, &path)) && !self.is_excluded(&path)
    }

    /// Does the view keep the file at `path`? True when the view
    /// [covers](ViewSpec::covers) it, or when it is a spine file in a
    /// directory the view passes through on the way to an included prefix.
    pub fn includes_file(&self, path: &str) -> bool {
        let path = normalize_prefix(path);
        if self.covers(&path) {
            return true;
        }
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        self.spine_files.iter().any(|s| s == name)
            && !self.is_excluded(&path)
            && self.dir_state(dir) != DirState::Outside
    }

    fn dir_state(&self, dir: &str) -> DirState {
        if self.is_excluded(dir) {
            return DirState::Outside;
        }
        if self.include.iter().any(|i| prefix_covers(i, dir)) {
            if self.exclude.iter().any(|e| prefix_covers(dir, e)) {
                DirState::Partial
            } else {
                DirState::Full
            }
        } else if self.include.iter().any(|i| prefix_covers(dir, i)) {
            DirState::Partial
        } else {
            DirState::Outside
        }
    }
}

fn normalize_all<I>(raw: I) -> Result<Vec<String>>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut out = Vec::new();
    for p in raw {
        let p = normalize_prefix(p.as_ref());
        if !p.is_empty() {
            validate_tree_path(&p)?;
        }
        out.push(p);
    }
    Ok(out)
}

/// Sort, dedup, and drop every prefix covered by another one.
fn drop_covered(mut prefixes: Vec<String>) -> Vec<String> {
    prefixes.sort();
    prefixes.dedup();
    let all = prefixes.clone();
    prefixes.retain(|p| !all.iter().any(|q| q != p && prefix_covers(q, p)));
    prefixes
}

/// Prune the tree at `root` to `view`. Directories wholly in view keep their
/// hashes; `trees` holds only the spine directories pruning rebuilt (each
/// once). An empty result is [`Tree::empty_hash`].
pub fn prune_tree<F>(root: &Hash, view: &ViewSpec, fetch: &mut F) -> Result<BuiltTree>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let mut trees = Vec::new();
    let mut seen = HashSet::new();
    let root_hash = match dir_hash(root) {
        Some(root) => prune_dir(root, "", view, fetch, &mut trees, &mut seen)?,
        None => None,
    };
    Ok(BuiltTree {
        root_hash: root_hash.unwrap_or_else(Tree::empty_hash),
        trees,
    })
}

fn prune_dir<F>(
    dir: &Hash,
    path: &str,
    view: &ViewSpec,
    fetch: &mut F,
    trees: &mut Vec<Tree>,
    seen: &mut HashSet<Hash>,
) -> Result<Option<Hash>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    match view.dir_state(path) {
        DirState::Outside => return Ok(None),
        DirState::Full => return Ok(Some(dir.clone())),
        DirState::Partial => {}
    }
    let tree = load_tree(dir, fetch)?;
    let mut kept = Vec::with_capacity(tree.entries.len());
    for entry in &tree.entries {
        let child = child_path(path, &entry.name);
        match entry.kind {
            TreeEntryKind::Blob => {
                if view.includes_file(&child) {
                    kept.push(entry.clone());
                }
            }
            TreeEntryKind::Tree => {
                if let Some(hash) = prune_dir(&entry.hash, &child, view, fetch, trees, seen)? {
                    kept.push(TreeEntry {
                        hash,
                        ..entry.clone()
                    });
                }
            }
        }
    }
    finish_dir(Some(&tree), kept, trees, seen)
}

/// The hash for a rebuilt directory: `None` when empty, the original hash
/// when nothing changed, otherwise a new tree recorded once in `trees`.
fn finish_dir(
    original: Option<&Tree>,
    entries: Vec<TreeEntry>,
    trees: &mut Vec<Tree>,
    seen: &mut HashSet<Hash>,
) -> Result<Option<Hash>> {
    if entries.is_empty() {
        return Ok(None);
    }
    if let Some(original) = original {
        let unchanged = original.entries.len() == entries.len()
            && original.entries.iter().zip(&entries).all(|(a, b)| {
                a.name == b.name && a.hash == b.hash && a.kind == b.kind && a.mode == b.mode
            });
        if unchanged {
            return Ok(Some(original.hash.clone()));
        }
    }
    let tree = Tree::new(entries)?;
    let hash = tree.hash.clone();
    if seen.insert(hash.clone()) {
        trees.push(tree);
    }
    Ok(Some(hash))
}

/// Splice the view tree `view_root` back into the full tree `full_root`:
/// everything the view selects is taken from `view_root` (so a file missing
/// there is a deletion), everything else from `full_root`. `fetch` must
/// resolve trees of both sides.
///
/// Refuses, with [`OakError::InvalidPath`] naming the path, any entry in
/// `view_root` the view doesn't select, and a file and a directory meeting
/// at one path (an in-view file replacing an out-of-view directory, or the
/// reverse). `trees` holds only the directories the graft rebuilt.
///
/// For a `view_root` produced by [`prune_tree`] over `full_root` this is the
/// identity, and `prune_tree(graft_tree(f, v)) == v` for any valid `v`.
pub fn graft_tree<F>(
    full_root: &Hash,
    view_root: &Hash,
    view: &ViewSpec,
    fetch: &mut F,
) -> Result<BuiltTree>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    let mut trees = Vec::new();
    let mut seen = HashSet::new();
    let root_hash = graft_dir(
        dir_hash(full_root),
        dir_hash(view_root),
        "",
        view,
        fetch,
        &mut trees,
        &mut seen,
    )?;
    Ok(BuiltTree {
        root_hash: root_hash.unwrap_or_else(Tree::empty_hash),
        trees,
    })
}

fn outside_view(path: &str) -> OakError {
    let shown = if path.is_empty() { "/" } else { path };
    OakError::InvalidPath(format!("{shown} is outside this view"))
}

#[allow(clippy::too_many_arguments)]
fn graft_dir<F>(
    full: Option<&Hash>,
    viewed: Option<&Hash>,
    path: &str,
    view: &ViewSpec,
    fetch: &mut F,
    trees: &mut Vec<Tree>,
    seen: &mut HashSet<Hash>,
) -> Result<Option<Hash>>
where
    F: FnMut(&Hash) -> Result<Tree>,
{
    match view.dir_state(path) {
        DirState::Outside => {
            return match viewed {
                Some(_) => Err(outside_view(path)),
                None => Ok(full.cloned()),
            }
        }
        DirState::Full => return Ok(viewed.cloned()),
        DirState::Partial => {}
    }
    let full_tree = full.map(|h| load_tree(h, fetch)).transpose()?;
    let view_tree = viewed.map(|h| load_tree(h, fetch)).transpose()?;

    // name -> (full entry, view entry)
    let mut names: BTreeMap<&str, (Option<&TreeEntry>, Option<&TreeEntry>)> = BTreeMap::new();
    for e in full_tree.iter().flat_map(|t| &t.entries) {
        names.entry(&e.name).or_default().0 = Some(e);
    }
    for e in view_tree.iter().flat_map(|t| &t.entries) {
        names.entry(&e.name).or_default().1 = Some(e);
    }

    let mut entries = Vec::with_capacity(names.len());
    for (name, (full_entry, view_entry)) in names {
        let child = child_path(path, name);
        let tree_of = |e: Option<&TreeEntry>| {
            e.filter(|e| e.kind == TreeEntryKind::Tree)
                .map(|e| e.hash.clone())
        };
        let blob_of = |e: Option<&TreeEntry>| e.filter(|e| e.kind == TreeEntryKind::Blob).cloned();

        let sub = graft_dir(
            tree_of(full_entry).as_ref(),
            tree_of(view_entry).as_ref(),
            &child,
            view,
            fetch,
            trees,
            seen,
        )?;
        let file = if view.includes_file(&child) {
            blob_of(view_entry)
        } else {
            if blob_of(view_entry).is_some() {
                return Err(outside_view(&child));
            }
            blob_of(full_entry)
        };
        match (sub, file) {
            (Some(_), Some(_)) => {
                return Err(OakError::InvalidPath(format!(
                    "{child} would be both a file and a directory \
                     (one of them is outside this view)"
                )))
            }
            (Some(hash), None) => entries.push(TreeEntry {
                name: name.to_string(),
                kind: TreeEntryKind::Tree,
                hash,
                mode: crate::FileMode::Regular,
            }),
            (None, Some(blob)) => entries.push(blob),
            (None, None) => {}
        }
    }
    finish_dir(full_tree.as_ref(), entries, trees, seen)
}

/// What a source commit becomes in a view.
#[derive(Debug, Clone)]
pub enum DerivedViewCommit {
    /// The source commit changed nothing the view selects: it maps to this
    /// existing view commit (the view parent's), so a view's history only
    /// holds commits that touched it.
    Reuse(Hash),
    /// A new view commit.
    New(Commit),
}

impl DerivedViewCommit {
    /// The view commit hash the source commit maps to.
    pub fn hash(&self) -> &Hash {
        match self {
            DerivedViewCommit::Reuse(hash) => hash,
            DerivedViewCommit::New(commit) => &commit.hash,
        }
    }
}

/// Derive the view commit for `source`, given its pruned root and the view
/// commits its parents map to.
///
/// The result is an ordinary [`Commit`] — same hash format, same
/// verification — with `manifest_hash` set to the pruned root, parents set
/// to the parents' view commits, branch, author and timestamp copied, and
/// `files` filtered to the paths the view keeps. When `redact_message` is
/// set (the source also changed paths the viewer can't read) the message is
/// replaced with [`REDACTED_VIEW_MESSAGE`].
///
/// A source that leaves the view unchanged — same pruned root as its first
/// parent's view commit, and no merge parent bringing in a different view
/// commit — is [`DerivedViewCommit::Reuse`] of that parent.
pub fn derive_view_commit(
    source: &Commit,
    view: &ViewSpec,
    view_root: Hash,
    view_parent: Option<&Commit>,
    view_merge_parent: Option<Hash>,
    redact_message: bool,
) -> Result<DerivedViewCommit> {
    if let Some(parent) = view_parent {
        let merge_is_noop = view_merge_parent
            .as_ref()
            .is_none_or(|mp| mp == &parent.hash);
        if parent.manifest_hash == view_root && merge_is_noop {
            return Ok(DerivedViewCommit::Reuse(parent.hash.clone()));
        }
    }
    let files: Vec<FileChange> = source
        .files
        .iter()
        .filter(|f| {
            view.includes_file(&f.path)
                || f.old_path.as_deref().is_some_and(|p| view.includes_file(p))
        })
        .cloned()
        .collect();
    let message = match &source.message {
        Some(_) if redact_message => Some(REDACTED_VIEW_MESSAGE.to_string()),
        other => other.clone(),
    };
    let merge_parent = view_merge_parent.filter(|mp| view_parent.is_none_or(|p| &p.hash != mp));
    Commit::with_timestamp(
        source.branch_name.clone(),
        view_parent.map(|p| p.hash.clone()),
        merge_parent,
        view_root,
        source.author.clone(),
        message,
        files,
        source.timestamp,
    )
    .map(DerivedViewCommit::New)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::test_support::{mutate, random_entries, Rng, Store};
    use crate::tree::{build_tree, flatten_tree};
    use crate::{hash_string, FileMode, ManifestEntry};

    fn entry(path: &str, content: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string(content),
            mode: FileMode::Regular,
        }
    }

    fn view(include: &[&str], exclude: &[&str], spine: &[&str]) -> ViewSpec {
        ViewSpec::new(include, exclude, spine).unwrap()
    }

    fn flat(store: &Store, root: &Hash) -> Vec<ManifestEntry> {
        let mut out = flatten_tree(root, &mut |h: &Hash| store.fetch(h)).unwrap();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn sorted(mut entries: Vec<ManifestEntry>) -> Vec<ManifestEntry> {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }

    fn paths(entries: &[ManifestEntry]) -> Vec<(String, Hash, FileMode)> {
        entries
            .iter()
            .map(|e| (e.path.clone(), e.blob_hash.clone(), e.mode))
            .collect()
    }

    fn company() -> Vec<ManifestEntry> {
        vec![
            entry("AGENTS.md", "root agents"),
            entry("README.md", "readme"),
            entry("apps/AGENTS.md", "apps agents"),
            entry("apps/web/src/main.ts", "web"),
            entry("apps/web/config/prod/secret.env", "secret"),
            entry("apps/billing/src/lib.rs", "billing"),
            entry("libs/ui/button.tsx", "button"),
            entry("libs/auth/login.rs", "login"),
        ]
    }

    #[test]
    fn normalizes_redundant_entries() {
        let v = view(
            &["/apps/web/", "apps/web/src", "libs/ui", "vendor/x"],
            &[
                "apps/web/config",
                "apps/web/config/prod",
                "elsewhere",
                "vendor",
            ],
            &["CLAUDE.md", "AGENTS.md", "AGENTS.md"],
        );
        assert_eq!(v.include(), &["apps/web", "libs/ui"]);
        assert_eq!(v.exclude(), &["apps/web/config"]);
        assert_eq!(v.spine_files(), &["AGENTS.md", "CLAUDE.md"]);
    }

    #[test]
    fn rejects_empty_and_whole_repo_exclude() {
        assert!(ViewSpec::new(["a"], ["a"], [] as [&str; 0]).is_err());
        assert!(ViewSpec::new([] as [&str; 0], [] as [&str; 0], [] as [&str; 0]).is_err());
        assert!(ViewSpec::new([""], ["/"], [] as [&str; 0]).is_err());
        assert!(ViewSpec::new(["a"], [] as [&str; 0], ["a/b"]).is_err());
        assert!(ViewSpec::new(["a/../b"], [] as [&str; 0], [] as [&str; 0]).is_err());
    }

    #[test]
    fn canonical_form_round_trips_and_ids_are_stable() {
        let v = view(
            &["libs/ui", "apps/web"],
            &["apps/web/config"],
            &["AGENTS.md"],
        );
        assert_eq!(
            v.to_canonical(),
            "+apps/web\n+libs/ui\n-apps/web/config\n=AGENTS.md"
        );
        let parsed = ViewSpec::parse(&v.to_canonical()).unwrap();
        assert_eq!(parsed, v);
        assert_eq!(parsed.id(), v.id());
        let reordered = view(
            &["apps/web/", "libs/ui"],
            &["/apps/web/config"],
            &["AGENTS.md"],
        );
        assert_eq!(reordered.id(), v.id());
        assert_ne!(view(&["apps/web"], &[], &[]).id(), v.id());
        assert_eq!(ViewSpec::parse("+").unwrap(), ViewSpec::full());
        assert!(ViewSpec::parse("apps").is_err());
    }

    #[test]
    fn union_keeps_only_shared_exclusions() {
        let a = view(&["apps"], &["apps/web/config"], &[]);
        let b = view(&["apps/web", "libs/ui"], &[], &["AGENTS.md"]);
        let u = a.union(&b).unwrap();
        assert_eq!(u.include(), &["apps", "libs/ui"]);
        assert!(u.exclude().is_empty());
        assert_eq!(u.spine_files(), &["AGENTS.md"]);
        let c = view(&["libs"], &[], &[]);
        assert_eq!(a.union(&c).unwrap().exclude(), &["apps/web/config"]);
    }

    #[test]
    fn prune_drops_sibling_names_and_keeps_spine_files() {
        let mut store = Store::default();
        let root = store.add_entries(&company());
        let v = view(
            &["apps/web", "libs/ui"],
            &["apps/web/config/prod"],
            &DEFAULT_SPINE_FILES,
        );
        let pruned = prune_tree(&root, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
        store.add_trees(pruned.trees);
        let got: Vec<String> = flat(&store, &pruned.root_hash)
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(
            got,
            [
                "AGENTS.md",
                "apps/AGENTS.md",
                "apps/web/src/main.ts",
                "libs/ui/button.tsx"
            ]
        );
    }

    #[test]
    fn prune_reuses_in_view_subtree_hashes() {
        let mut store = Store::default();
        let root = store.add_entries(&company());
        let fetch = |h: &Hash| store.fetch(h);
        let ui_before = crate::subtree_at_path(&root, "libs/ui", &mut { fetch }).unwrap();
        let v = view(&["libs/ui", "apps/web"], &[], &[]);
        let pruned = prune_tree(&root, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
        // Rebuilt: root, libs, apps. Not libs/ui or apps/web (reused by hash).
        assert_eq!(pruned.trees.len(), 3);
        store.add_trees(pruned.trees.clone());
        let ui_after =
            crate::subtree_at_path(&pruned.root_hash, "libs/ui", &mut |h: &Hash| store.fetch(h))
                .unwrap();
        assert_eq!(ui_before, ui_after);
    }

    #[test]
    fn full_view_is_identity() {
        let mut store = Store::default();
        let root = store.add_entries(&company());
        let pruned = prune_tree(&root, &ViewSpec::full(), &mut |h: &Hash| store.fetch(h)).unwrap();
        assert_eq!(pruned.root_hash, root);
        assert!(pruned.trees.is_empty());
    }

    #[test]
    fn graft_refuses_writes_outside_the_view() {
        let mut store = Store::default();
        let full = company();
        let root = store.add_entries(&full);
        let v = view(&["apps/web"], &[], &DEFAULT_SPINE_FILES);

        let mut outside = full
            .iter()
            .filter(|e| v.includes_file(&e.path))
            .cloned()
            .collect::<Vec<_>>();
        outside.push(entry("apps/new.txt", "sneaky"));
        let view_root = store.add_entries(&outside);
        let err = graft_tree(&root, &view_root, &v, &mut |h: &Hash| store.fetch(h)).unwrap_err();
        assert!(
            err.to_string()
                .contains("apps/new.txt is outside this view"),
            "{err}"
        );

        let mut outside_dir = full
            .iter()
            .filter(|e| v.includes_file(&e.path))
            .cloned()
            .collect::<Vec<_>>();
        outside_dir.push(entry("libs/auth/evil.rs", "x"));
        let view_root = store.add_entries(&outside_dir);
        let err = graft_tree(&root, &view_root, &v, &mut |h: &Hash| store.fetch(h)).unwrap_err();
        assert!(
            err.to_string().contains("libs is outside this view"),
            "{err}"
        );
    }

    #[test]
    fn graft_applies_in_view_edits_and_spine_edits() {
        let mut store = Store::default();
        let full = company();
        let root = store.add_entries(&full);
        let v = view(&["apps/web"], &[], &DEFAULT_SPINE_FILES);
        let mut edited: Vec<ManifestEntry> = full
            .iter()
            .filter(|e| v.includes_file(&e.path) && e.path != "apps/web/src/main.ts")
            .cloned()
            .collect();
        edited.push(entry("apps/web/src/app.ts", "new"));
        edited.retain(|e| e.path != "AGENTS.md");
        edited.push(entry("AGENTS.md", "edited root agents"));
        let view_root = store.add_entries(&edited);
        let grafted = graft_tree(&root, &view_root, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
        store.add_trees(grafted.trees);

        let mut expected: Vec<ManifestEntry> = full
            .iter()
            .filter(|e| !v.includes_file(&e.path))
            .cloned()
            .collect();
        expected.extend(edited);
        assert_eq!(
            paths(&flat(&store, &grafted.root_hash)),
            paths(&sorted(expected))
        );
    }

    fn random_view(rng: &mut Rng) -> ViewSpec {
        const PREFIXES: [&str; 9] = ["", "a", "b", "c", "a/b", "a/c", "b/a", "c/d", "a/b/c"];
        loop {
            let pick = |rng: &mut Rng| PREFIXES[rng.below(PREFIXES.len() as u64) as usize];
            let include: Vec<&str> = (0..1 + rng.below(3)).map(|_| pick(rng)).collect();
            let exclude: Vec<&str> = (0..rng.below(3))
                .map(|_| pick(rng))
                .filter(|p| !p.is_empty())
                .collect();
            let spine: Vec<&str> = if rng.below(2) == 0 { vec!["d"] } else { vec![] };
            if let Ok(v) = ViewSpec::new(include, exclude, spine) {
                return v;
            }
        }
    }

    #[test]
    fn prune_matches_flat_filter_on_random_trees() {
        let mut rng = Rng(0x5eed_0001);
        for _ in 0..500 {
            let mut store = Store::default();
            let full = random_entries(&mut rng, 12);
            let root = store.add_entries(&full);
            let v = random_view(&mut rng);
            let pruned = prune_tree(&root, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
            store.add_trees(pruned.trees);
            let expected: Vec<ManifestEntry> = full
                .iter()
                .filter(|e| v.includes_file(&e.path))
                .cloned()
                .collect();
            assert_eq!(
                paths(&flat(&store, &pruned.root_hash)),
                paths(&sorted(expected)),
                "view {}",
                v.to_canonical()
            );
            // Pruning a full tree then grafting it back changes nothing.
            let back =
                graft_tree(&root, &pruned.root_hash, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
            assert_eq!(back.root_hash, root, "view {}", v.to_canonical());
        }
    }

    #[test]
    fn graft_matches_flat_splice_on_random_edits() {
        let mut rng = Rng(0x5eed_0002);
        let mut checked = 0;
        for _ in 0..800 {
            let mut store = Store::default();
            let full = random_entries(&mut rng, 12);
            let root = store.add_entries(&full);
            let v = random_view(&mut rng);
            let in_view: Vec<ManifestEntry> = full
                .iter()
                .filter(|e| v.includes_file(&e.path))
                .cloned()
                .collect();
            let edited: Vec<ManifestEntry> = mutate(&mut rng, &in_view, 4)
                .into_iter()
                .filter(|e| v.includes_file(&e.path))
                .collect();
            let view_root = store.add_entries(&edited);

            let mut expected: Vec<ManifestEntry> = full
                .iter()
                .filter(|e| !v.includes_file(&e.path))
                .cloned()
                .collect();
            expected.extend(edited.iter().cloned());
            let grafted = graft_tree(&root, &view_root, &v, &mut |h: &Hash| store.fetch(h));
            match build_tree(&expected) {
                Ok(built) => {
                    let grafted = grafted.unwrap_or_else(|e| {
                        panic!("view {}: unexpected graft error {e}", v.to_canonical())
                    });
                    assert_eq!(
                        grafted.root_hash,
                        built.root_hash,
                        "view {}",
                        v.to_canonical()
                    );
                    store.add_trees(grafted.trees);
                    // And the graft prunes back to exactly the view tree.
                    let again =
                        prune_tree(&grafted.root_hash, &v, &mut |h: &Hash| store.fetch(h)).unwrap();
                    assert_eq!(again.root_hash, view_root, "view {}", v.to_canonical());
                    checked += 1;
                }
                // A file/directory clash across the view boundary.
                Err(_) => assert!(grafted.is_err(), "view {}", v.to_canonical()),
            }
        }
        assert!(checked > 400, "too few clash-free cases: {checked}");
    }

    fn commit(manifest: &Hash, parent: Option<&Commit>, message: Option<&str>) -> Commit {
        Commit::with_timestamp(
            "main".to_string(),
            parent.map(|p| p.hash.clone()),
            None,
            manifest.clone(),
            "alice".to_string(),
            message.map(str::to_string),
            Vec::new(),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn view_commits_reuse_unchanged_parents_and_redact_messages() {
        let v = view(&["apps/web"], &[], &[]);
        let root_a = hash_string("pruned-a");
        let root_b = hash_string("pruned-b");
        let source = commit(&hash_string("full-1"), None, Some("touch web and billing"));

        let first =
            match derive_view_commit(&source, &v, root_a.clone(), None, None, false).unwrap() {
                DerivedViewCommit::New(c) => c,
                other => panic!("expected new, got {other:?}"),
            };
        assert_eq!(first.manifest_hash, root_a);
        assert_eq!(first.message.as_deref(), Some("touch web and billing"));
        assert_ne!(first.hash, source.hash);
        first.verify_fields().unwrap();

        // Same pruned root: maps onto the parent view commit.
        let next = commit(&hash_string("full-2"), Some(&source), Some("billing only"));
        let reused =
            derive_view_commit(&next, &v, root_a.clone(), Some(&first), None, false).unwrap();
        assert!(matches!(&reused, DerivedViewCommit::Reuse(h) if h == &first.hash));

        // Changed pruned root and touched restricted paths: new, redacted.
        let redacted = derive_view_commit(&next, &v, root_b, Some(&first), None, true).unwrap();
        let DerivedViewCommit::New(redacted) = redacted else {
            panic!("expected new");
        };
        assert_eq!(redacted.parent_hash.as_ref(), Some(&first.hash));
        assert_eq!(redacted.message.as_deref(), Some(REDACTED_VIEW_MESSAGE));
        redacted.verify_fields().unwrap();
    }

    #[test]
    fn view_commit_files_are_filtered_to_the_view() {
        use crate::{ChangeType, FileChange};
        let v = view(&["apps/web"], &[], &[]);
        let change = |path: &str| FileChange {
            path: path.to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: None,
            new_blob_hash: None,
            old_path: None,
            old_mode: None,
            new_mode: None,
        };
        let mut source = commit(&hash_string("full"), None, None);
        source.files = vec![change("apps/web/a.ts"), change("apps/billing/b.rs")];
        let DerivedViewCommit::New(c) =
            derive_view_commit(&source, &v, hash_string("p"), None, None, false).unwrap()
        else {
            panic!("expected new");
        };
        let files: Vec<&str> = c.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, ["apps/web/a.ts"]);
    }
}
