//! Tree-level merges for the CLI's local merge flows: `oak merge` into a
//! local parent, `oak pull`'s parent sync, and the divergence re-parent.
//!
//! These flows used to flatten the base, branch and parent manifests, merge
//! them path by path, rebuild the result's whole tree, and rewrite the working
//! tree from it — O(files) for a one-file change. They now merge root trees
//! with [`oak_core::three_way_merge_trees`], which takes unchanged and
//! one-sided directories whole by hash, patch conflict resolutions in with
//! [`oak_core::edit_tree`], and update the working tree from a tree diff of
//! the old head and the merged root.
//!
//! Completeness is checked where the merge reads: a missing tree on a walked
//! path is incomplete manifest data, a missing blob in a conflict triple is
//! incomplete blob data (raised by the conflict resolver), and every blob the
//! working-tree update writes is checked before the first file is touched.
//! Subtrees and blobs the merge never reads — out-of-cone content, history
//! the branch didn't change — are never required.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oak_core::{
    FileChange, Hash, Manifest, ManifestEntry, MergeConflict, MergeOutcome, OakError, Repository,
    Result, Tree, TreeEntryKind,
};

use crate::materialize::{apply_manifest, ApplyOpts, ApplyReport, DeleteScope, MissingBlobs};
use crate::workdir_lock::WorkdirLock;

/// A snapshot as trees: its root, plus the tree objects under it that aren't
/// stored yet.
pub(crate) struct MergedTree {
    pub root: Hash,
    pub trees: Vec<Tree>,
}

impl MergedTree {
    /// A snapshot given as a flat entry list — the `--continue` paths, whose
    /// result comes from scanning the working tree.
    pub(crate) fn from_entries(entries: &[ManifestEntry]) -> Result<Self> {
        let built = oak_core::build_tree(entries)?;
        Ok(MergedTree {
            root: built.root_hash,
            trees: built.trees,
        })
    }

    /// Store the pending trees, for state that must outlive this process (a
    /// paused merge's recorded result).
    pub(crate) fn store(&self, repo: &dyn Repository) -> Result<()> {
        for tree in &self.trees {
            repo.store_tree(tree)?;
        }
        Ok(())
    }
}

/// A tree-level merge with the CLI's conflict policy applied: text conflicts
/// carry markers, binary conflicts keep the branch side, modify/delete keeps
/// the survivor (see [`super::merge::resolve_conflicts_with_markers`]).
pub(crate) struct TreeMerge {
    pub merged: MergedTree,
    pub conflict_paths: Vec<String>,
    pub binary_conflict_paths: Vec<String>,
}

impl TreeMerge {
    pub(crate) fn conflict_count(&self) -> usize {
        self.conflict_paths.len() + self.binary_conflict_paths.len()
    }
}

/// Trees built by a merge and not stored yet, by hash.
type Pending<'a> = HashMap<&'a Hash, &'a Tree>;

fn pending(trees: &[Tree]) -> Pending<'_> {
    trees.iter().map(|tree| (&tree.hash, tree)).collect()
}

/// Fetch a tree from `pending` or the repository. A missing tree is
/// incomplete manifest data between `left` and `right`.
fn fetch_pending(
    repo: &dyn Repository,
    pending: &Pending<'_>,
    hash: &Hash,
    left: &str,
    right: &str,
) -> Result<Tree> {
    match pending.get(hash) {
        Some(tree) => Ok((*tree).clone()),
        None => fetch_tree(repo, hash, left, right),
    }
}

/// Fetch a stored tree. A missing tree is incomplete manifest data between
/// `left` and `right`.
pub(crate) fn fetch_tree(
    repo: &dyn Repository,
    hash: &Hash,
    left: &str,
    right: &str,
) -> Result<Tree> {
    repo.get_tree(hash)?.ok_or_else(|| {
        super::merge::incomplete_manifest_error(left, right, std::slice::from_ref(hash))
    })
}

/// Require a snapshot's root tree to be stored locally. Only the root: the
/// merge checks the subtrees it walks as it reaches them.
pub(crate) fn require_root(
    repo: &dyn Repository,
    root: &Hash,
    left: &str,
    right: &str,
) -> Result<()> {
    if repo.has_tree(root)? {
        Ok(())
    } else {
        Err(super::merge::incomplete_manifest_error(
            left,
            right,
            std::slice::from_ref(root),
        ))
    }
}

/// Three-way merge `branch_root` and `parent_root` against `base_root`, then
/// resolve conflicts with the CLI policy and patch the resolutions into the
/// merged tree. Reads only the directories the three sides disagree on, the
/// blobs of conflicting paths, and the new trees are returned unstored.
pub(crate) fn merge_roots(
    repo: &dyn Repository,
    base_root: &Hash,
    branch_root: &Hash,
    parent_root: &Hash,
    branch_name: &str,
    parent_name: &str,
) -> Result<TreeMerge> {
    let mut fetch = |hash: &Hash| fetch_tree(repo, hash, branch_name, parent_name);
    let outcome = oak_core::three_way_merge_trees(base_root, branch_root, parent_root, &mut fetch)?;
    let resolution =
        resolve_tree_conflicts(repo, base_root, outcome.conflicts, branch_name, parent_name)?;
    let created = outcome.trees;
    let edited = {
        let index = pending(&created);
        let mut fetch = |hash: &Hash| fetch_pending(repo, &index, hash, branch_name, parent_name);
        oak_core::edit_tree(&outcome.root, &resolution.entries, &[], &mut fetch)?
    };
    let mut trees = created;
    trees.extend(edited.trees);
    Ok(TreeMerge {
        merged: MergedTree {
            trees: reachable_new_trees(&edited.root_hash, trees),
            root: edited.root_hash,
        },
        conflict_paths: resolution.conflict_paths,
        binary_conflict_paths: resolution.binary_conflict_paths,
    })
}

/// [`super::merge::resolve_conflicts_with_markers`] with each conflict's base
/// entry looked up in the base tree (O(depth) per conflict).
fn resolve_tree_conflicts(
    repo: &dyn Repository,
    base_root: &Hash,
    conflicts: Vec<MergeConflict>,
    branch_name: &str,
    parent_name: &str,
) -> Result<super::merge::ConflictResolution> {
    let mut fetch = |hash: &Hash| fetch_tree(repo, hash, branch_name, parent_name);
    let mut base_entry = |path: &str| -> Result<Option<ManifestEntry>> {
        Ok(match oak_core::resolve_path(base_root, path, &mut fetch)? {
            Some(entry) if entry.kind == TreeEntryKind::Blob => Some(ManifestEntry {
                path: path.to_string(),
                blob_hash: entry.hash,
                mode: entry.mode,
            }),
            _ => None,
        })
    };
    super::merge::resolve_conflicts_with_markers(
        repo,
        &mut base_entry,
        conflicts,
        branch_name,
        parent_name,
    )
}

/// The trees in `new` reachable from `root`. Every tree above a new tree is
/// new too (its hash covers the child's), so walking only through `new`
/// finds them all.
fn reachable_new_trees(root: &Hash, new: Vec<Tree>) -> Vec<Tree> {
    let mut by_hash: HashMap<Hash, Tree> = new.into_iter().map(|t| (t.hash.clone(), t)).collect();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(hash) = stack.pop() {
        if let Some(tree) = by_hash.remove(&hash) {
            stack.extend(
                tree.entries
                    .iter()
                    .filter(|e| e.kind == TreeEntryKind::Tree)
                    .map(|e| e.hash.clone()),
            );
            out.push(tree);
        }
    }
    out
}

/// The commit file list from `old_root` to `new_root`: the same changes, and
/// the same mode-less rows, as `Manifest::diff` over the flattened snapshots.
pub(crate) fn commit_file_changes(
    repo: &dyn Repository,
    created: &[Tree],
    old_root: &Hash,
    new_root: &Hash,
    left: &str,
    right: &str,
) -> Result<Vec<FileChange>> {
    let index = pending(created);
    let mut fetch = |hash: &Hash| fetch_pending(repo, &index, hash, left, right);
    let changes = oak_core::diff_trees(
        Some(old_root),
        Some(new_root),
        &mut fetch,
        &oak_core::RenameConfig::default(),
    )?;
    Ok(changes
        .into_iter()
        .map(|c| FileChange {
            old_mode: None,
            new_mode: None,
            ..c
        })
        .collect())
}

/// What [`apply_tree_diff`] does with paths the new snapshot dropped.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removed {
    /// Delete them (when unmodified files, never directories) and prune the
    /// directories that empties — a merge landing.
    Delete,
    /// Leave them: a conflict write-out, whose deletions land at
    /// `--continue`.
    Keep,
}

/// Moving a working tree that holds one snapshot to another by writing only
/// the files that differ — the tree-diff counterpart of resetting to the new
/// manifest. Files the change doesn't touch, untracked files, and their
/// stat-cache rows are left alone.
pub(crate) struct WorktreeDiff {
    /// Files to write: every path whose entry differs in the new snapshot.
    target: Manifest,
    /// Tracked files the new snapshot dropped.
    dropped: Manifest,
    removed: Removed,
}

impl WorktreeDiff {
    /// Plan the move from `old_root` to `new_root` and check that every blob
    /// it writes (within the sparse cone) is local, so a caller can verify
    /// before publishing refs that the update after them can't stop halfway.
    /// `created` holds trees under `new_root` that aren't stored yet.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan(
        repo: &dyn Repository,
        created: &[Tree],
        old_root: &Hash,
        new_root: &Hash,
        removed: Removed,
        left: &str,
        right: &str,
    ) -> Result<Self> {
        let index = pending(created);
        let mut fetch = |hash: &Hash| fetch_pending(repo, &index, hash, left, right);
        let plan = Self::between(old_root, new_root, removed, &mut fetch)?;
        crate::materialize::require_blobs_for_apply(repo, &plan.target)?;
        Ok(plan)
    }

    /// Plan the move from `old_root` to `new_root` without checking blobs,
    /// for callers with their own missing-content policy (`oak pull`).
    /// `fetch` reads both sides' trees and decides how a missing one is
    /// reported.
    pub(crate) fn between<F>(
        old_root: &Hash,
        new_root: &Hash,
        removed: Removed,
        fetch: &mut F,
    ) -> Result<Self>
    where
        F: FnMut(&Hash) -> Result<Tree>,
    {
        let (old_side, new_side) = if old_root == new_root {
            (Vec::new(), Vec::new())
        } else {
            oak_core::diff_tree_entries(Some(old_root), Some(new_root), fetch)?
        };
        let written: HashSet<&str> = new_side.iter().map(|e| e.path.as_str()).collect();
        let dropped: Vec<ManifestEntry> = old_side
            .iter()
            .filter(|e| !written.contains(e.path.as_str()))
            .cloned()
            .collect();
        Ok(WorktreeDiff {
            target: Manifest {
                hash: new_root.clone(),
                entries: new_side,
            },
            dropped: Manifest {
                hash: old_root.clone(),
                entries: dropped,
            },
            removed,
        })
    }

    /// Write every file of the new snapshot (`entries`, its flattened form),
    /// not only the ones that differ, still removing only the dropped paths.
    /// For a working tree not known to hold the old snapshot: O(files).
    pub(crate) fn rewrite_everything(mut self, entries: Vec<ManifestEntry>) -> Self {
        self.target.entries = entries;
        self
    }

    /// The files [`Self::apply`] writes.
    pub(crate) fn target(&self) -> &Manifest {
        &self.target
    }

    /// Write the planned files (and, for [`Removed::Delete`], remove the
    /// dropped ones). Blobs are checked again before the first write.
    pub(crate) fn apply(
        &self,
        lock: &WorkdirLock,
        work_tree: &Path,
        repo: &dyn Repository,
    ) -> Result<()> {
        self.apply_with(lock, work_tree, repo, MissingBlobs::Error, None)
            .map(|_| ())
    }

    /// [`Self::apply`] with an explicit missing-blob policy: with
    /// [`MissingBlobs::Skip`], files whose blob isn't local are left out and
    /// listed in the report, and those in `remove_missing_paths` are also
    /// removed from disk (see [`ApplyOpts::remove_missing_paths`]).
    pub(crate) fn apply_with(
        &self,
        lock: &WorkdirLock,
        work_tree: &Path,
        repo: &dyn Repository,
        missing_blobs: MissingBlobs,
        remove_missing_paths: Option<&HashSet<String>>,
    ) -> Result<ApplyReport> {
        if self.target.entries.is_empty() && self.dropped.entries.is_empty() {
            return Ok(ApplyReport::default());
        }
        let opts = match self.removed {
            Removed::Delete => ApplyOpts {
                delete: DeleteScope::TrackedRemoved { old: &self.dropped },
                missing_blobs,
                remove_missing_paths,
                clean_empty_dirs: true,
                ..ApplyOpts::default()
            },
            Removed::Keep => ApplyOpts {
                delete: DeleteScope::Nothing,
                missing_blobs,
                remove_missing_paths,
                clean_empty_dirs: false,
                ..ApplyOpts::default()
            },
        };
        apply_manifest(lock, work_tree, repo, &self.target, opts)
    }
}

/// [`WorktreeDiff::plan`] and apply in one step, for callers with no refs to
/// publish in between.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_tree_diff(
    lock: &WorkdirLock,
    work_tree: &Path,
    repo: &dyn Repository,
    created: &[Tree],
    old_root: &Hash,
    new_root: &Hash,
    removed: Removed,
    left: &str,
    right: &str,
) -> Result<()> {
    WorktreeDiff::plan(repo, created, old_root, new_root, removed, left, right)?
        .apply(lock, work_tree, repo)
}

/// [`oak_core::three_way_merge_manifests`] over manifests already in memory
/// whose trees are stored (or in `unstored`, by hash), for read-only
/// predictions. The
/// tree-level merge finds the conflicts reading only the directories the
/// sides disagree on; the clean entries are `parent`'s entries patched by the
/// tree diff from its root to the merged root, instead of three path maps over
/// every file.
///
/// Falls back to the flat merge whenever the tree path can't run — a tree
/// missing locally, a manifest whose hash isn't a stored root, or a clean
/// result that clashes a file with a directory — so a prediction never
/// changes or fails because of how it was computed.
pub(crate) fn merge_manifests(
    repo: &dyn Repository,
    unstored: &HashMap<Hash, Tree>,
    base: &Manifest,
    branch: &Manifest,
    parent: &Manifest,
) -> MergeOutcome {
    merge_manifests_by_tree(repo, unstored, base, branch, parent)
        .unwrap_or_else(|_| oak_core::three_way_merge_manifests(base, branch, parent))
}

fn merge_manifests_by_tree(
    repo: &dyn Repository,
    unstored: &HashMap<Hash, Tree>,
    base: &Manifest,
    branch: &Manifest,
    parent: &Manifest,
) -> Result<MergeOutcome> {
    let fetch_from = |trees: &[Tree], hash: &Hash| -> Result<Tree> {
        if let Some(tree) = trees.iter().find(|t| &t.hash == hash) {
            return Ok(tree.clone());
        }
        if let Some(tree) = unstored.get(hash) {
            return Ok(tree.clone());
        }
        repo.get_tree(hash)?
            .ok_or_else(|| OakError::ManifestNotFound(hash.to_string()))
    };
    let outcome = oak_core::three_way_merge_trees(
        &base.hash,
        &branch.hash,
        &parent.hash,
        &mut |hash: &Hash| fetch_from(&[], hash),
    )?;
    let (old_side, new_side) = oak_core::diff_tree_entries(
        Some(&parent.hash),
        Some(&outcome.root),
        &mut |hash: &Hash| fetch_from(&outcome.trees, hash),
    )?;
    let changed: HashSet<&str> = old_side
        .iter()
        .chain(new_side.iter())
        .map(|e| e.path.as_str())
        .collect();
    let mut clean_entries: Vec<ManifestEntry> = parent
        .entries
        .iter()
        .filter(|e| !changed.contains(e.path.as_str()))
        .cloned()
        .collect();
    clean_entries.extend(new_side);
    Ok(MergeOutcome {
        clean_entries,
        conflicts: outcome.conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{FileMode, SqliteRepository};

    fn entry(path: &str, content: &str, mode: FileMode) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: oak_core::hash_string(content),
            mode,
        }
    }

    /// Sorted `(path, blob, mode)` of the clean entries and sorted conflict
    /// paths: the flat merge returns both in hash-set order.
    fn keys(outcome: &MergeOutcome) -> (Vec<String>, Vec<String>) {
        let mut clean: Vec<String> = outcome
            .clean_entries
            .iter()
            .map(|e| format!("{} {} {:?}", e.path, e.blob_hash, e.mode))
            .collect();
        clean.sort();
        let mut conflicts: Vec<String> = outcome.conflicts.iter().map(|c| c.path.clone()).collect();
        conflicts.sort();
        (clean, conflicts)
    }

    /// Deterministic pseudo-random sides over a small path alphabet, so
    /// edits, deletions, mode flips and file/directory swaps collide.
    fn sides(seed: u64) -> [Vec<ManifestEntry>; 3] {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        const PATHS: [&str; 7] = ["a", "b/c", "b/d", "e/f/g", "e/h", "i", "e/f/j"];
        let mut base: Vec<ManifestEntry> = Vec::new();
        for path in PATHS {
            if next(4) != 0 {
                let content = format!("v{}", next(3));
                base.push(entry(path, &content, FileMode::Regular));
            }
        }
        let mut mutate = |entries: &[ManifestEntry]| -> Vec<ManifestEntry> {
            let mut out: Vec<ManifestEntry> = Vec::new();
            for e in entries {
                match next(6) {
                    0 => {}
                    1 => out.push(entry(&e.path, &format!("v{}", next(3)), e.mode)),
                    2 => out.push(ManifestEntry {
                        mode: FileMode::Executable,
                        ..e.clone()
                    }),
                    _ => out.push(e.clone()),
                }
            }
            if next(3) == 0 {
                let path = PATHS[next(PATHS.len() as u64) as usize];
                if !out.iter().any(|e| {
                    e.path == path
                        || e.path.starts_with(&format!("{path}/"))
                        || path.starts_with(&format!("{}/", e.path))
                }) {
                    out.push(entry(path, &format!("v{}", next(3)), FileMode::Regular));
                }
            }
            out
        };
        let branch = mutate(&base);
        let parent = mutate(&base);
        [base, branch, parent]
    }

    #[test]
    fn merge_manifests_matches_the_flat_merge() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let mut conflicted = 0;
        for seed in 0..300u64 {
            let [base, branch, parent] = sides(seed);
            let stored = |entries: Vec<ManifestEntry>| {
                let hash = repo.put_manifest(entries).unwrap();
                repo.get_manifest(&hash).unwrap().unwrap()
            };
            let (base, branch, parent) = (stored(base), stored(branch), stored(parent));
            let flat = oak_core::three_way_merge_manifests(&base, &branch, &parent);
            let by_tree =
                merge_manifests_by_tree(&repo, &HashMap::new(), &base, &branch, &parent).unwrap();
            assert_eq!(keys(&by_tree), keys(&flat), "seed {seed}");
            conflicted += usize::from(!flat.conflicts.is_empty());
        }
        assert!(conflicted > 20, "only {conflicted} conflicted cases");
    }

    #[test]
    fn merge_manifests_falls_back_when_trees_are_not_stored() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let [base, branch, parent] = sides(7);
        let (base, branch, parent) = (
            Manifest::new(base),
            Manifest::new(branch),
            Manifest::new(parent),
        );
        assert!(merge_manifests_by_tree(&repo, &HashMap::new(), &base, &branch, &parent).is_err());
        assert_eq!(
            keys(&merge_manifests(
                &repo,
                &HashMap::new(),
                &base,
                &branch,
                &parent
            )),
            keys(&oak_core::three_way_merge_manifests(
                &base, &branch, &parent
            ))
        );
    }
}
