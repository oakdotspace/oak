//! Durable record of a working-tree update in flight.
//!
//! Rewriting the working tree from a manifest is not atomic: a process killed
//! part-way leaves files of two snapshots side by side. Nothing else records
//! that, so such a tree reads as ordinary uncommitted work and `oak commit`
//! would record it.
//!
//! [`begin`] writes `.oak/WORKTREE_UPDATE` before the first file changes and
//! [`WorktreeUpdate::finish`] removes it once the refs (or conflict state)
//! that explain the new tree are stored. While the record exists and its
//! writer is gone:
//!
//! - `oak status` / `oak agent state` / `oak conflict status` report it
//!   ([`pending`]),
//! - every command that takes the working-directory lock is refused
//!   ([`ensure_none`]), so nothing snapshots the torn tree,
//! - `oak pull --abort` (and `oak pull`) recover first ([`recover_at`]).
//!
//! Which way recovery goes follows from where the refs are (see
//! [`Recovery`]). It never guesses about content: the record lists, per path,
//! what the update replaced and what it writes, and a path is changed only
//! when the disk still holds exactly one of those two. Anything else is the
//! user's, is left in place, and is named in the report.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use oak_core::{FileMode, Hash, Manifest, ManifestEntry, OakError, Repository, Result};
use serde::{Deserialize, Serialize};

use crate::materialize::{ApplyOpts, DeleteScope, MissingBlobs};
use crate::workdir_lock::WorkdirLock;

const FILE: &str = "WORKTREE_UPDATE";
/// Where a working-tree file is written before it is renamed into place.
/// Inside `.oak/`, so a writer killed mid-file leaves nothing in the tree.
const TEMP_DIR: &str = "worktree-tmp";
const VERSION: u32 = 1;

/// What recovery does with the paths an interrupted update recorded.
///
/// The rule: an update whose refs move only after the tree is rolled back
/// (the refs still name the old snapshot); one whose refs already name the
/// new snapshot is completed; `Clear` is only for an update that writes
/// toward the snapshot the refs named all along and whose operation state
/// (if any) still says it is unfinished, so a partial result is honestly
/// just uncommitted state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Recovery {
    Rollback,
    Complete,
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Side {
    /// Something the update could not identify (a socket, say).
    Unknown,
    Missing,
    File {
        hash: Hash,
        mode: FileMode,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct PathChange {
    path: String,
    before: Side,
    after: Side,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    /// The `oak` subcommand that was running.
    operation: String,
    pid: u32,
    started_at: String,
    /// Refs when the update began. Recovery replays nothing once they moved.
    branch: Option<String>,
    head: Option<Hash>,
    #[serde(default)]
    branch_head: Option<Hash>,
    /// For an update that moves its refs before the tree: where they go.
    /// Recorded before they move, so recovery can tell which side of that
    /// move the process died on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refs_after: Option<Refs>,
    recovery: Recovery,
    /// The sparse cone (as stored metadata) from before a cone change:
    /// rolling back puts it back along with the files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sparse_paths: Option<String>,
    #[serde(default)]
    paths: Vec<PathChange>,
    /// Names this update. A temp file it has to leave in the working tree
    /// (see [`Staging`]) carries it, so recovery removes that file and no
    /// other.
    #[serde(default)]
    id: String,
    /// Set (by rewriting the record once) before the first temp file is
    /// created in the working tree instead of the staging directory.
    #[serde(default)]
    sibling_temps: bool,
}

/// The refs a working tree is read against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Refs {
    pub branch: Option<String>,
    pub head: Option<Hash>,
    pub branch_head: Option<Hash>,
}

impl Refs {
    fn current(repo: &dyn Repository) -> Result<Self> {
        let branch = repo.get_current_branch_name()?;
        Ok(Refs {
            branch_head: match &branch {
                Some(name) => repo.get_branch_head(name)?,
                None => None,
            },
            head: repo.get_head()?,
            branch,
        })
    }

    /// The checkout on `branch` at `head`.
    pub(crate) fn on_branch(branch: &str, head: &Hash) -> Self {
        Refs {
            branch: Some(branch.to_string()),
            head: Some(head.clone()),
            branch_head: Some(head.clone()),
        }
    }
}

/// What is in `.oak/WORKTREE_UPDATE`.
enum Record {
    Readable(Box<Journal>),
    /// Present but not a record this oak can act on: empty, truncated, not
    /// JSON, or written by a newer oak (`Some(version)`).
    Unusable(Option<u32>),
}

/// The paths an update writes, for [`begin`].
pub(crate) struct Plan<'a> {
    /// Files written.
    pub target: &'a Manifest,
    /// Tracked files removed.
    pub dropped: &'a [ManifestEntry],
    /// The old snapshot's entries at the paths `target` overwrites.
    pub replaced: &'a [ManifestEntry],
    /// For a sparse-cone change: the stored cone it replaces. The paths are
    /// then recorded whatever cone is stored, since the change moves it.
    pub sparse_paths_before: Option<String>,
    /// Store the content of a file the update overwrites without knowing it
    /// (an untracked file at an incoming path), so a rollback can put it
    /// back. Off for whole-tree plans, where that would read every file.
    pub capture_unknown: bool,
    /// For [`Recovery::Complete`] begun before its refs move: where they
    /// are about to go. `None` when they have already moved.
    pub refs_after: Option<Refs>,
    /// `replaced` was just read from the disk (a whole-tree capture), so it
    /// need not be read again.
    pub replaced_is_disk: bool,
    /// What happens to a path whose on-disk content is neither the old
    /// snapshot's nor the new one's.
    pub on_overlap: OnOverlap,
}

/// The policy for a path the transition writes whose on-disk content is
/// neither side of it: the user's uncommitted work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnOverlap {
    /// Refuse the whole transition before its first write
    /// ([`OakError::LocalChangesWouldBeOverwritten`], exit 4). The default
    /// for every transition the user did not ask to discard work with.
    Refuse,
    /// Overwrite: the user asked for exactly that (`--clean`, `--force`), or
    /// a clean guard already proved there is nothing to lose and the old
    /// snapshot is not available to classify against.
    Overwrite,
}

/// Why a path is an [`Overlap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlapKind {
    /// A tracked file with an uncommitted edit, which the transition rewrites.
    Modified,
    /// A tracked file with an uncommitted edit, which the transition removes.
    ModifiedRemovedByUpdate,
    /// A tracked file deleted locally, which the transition rewrites.
    Deleted,
    /// An untracked file at a path the transition creates.
    Untracked,
    /// An ignored file at a path the transition creates.
    Ignored,
    /// An untracked file whose name differs only in case or Unicode
    /// normalization from a path the transition creates: on a filesystem that
    /// folds either (APFS, HFS+) the two names are one file. Listed under the
    /// spelling on disk.
    CaseTwin,
    /// Something that is not a file, symlink or directory at a path the
    /// transition creates.
    NotAFile,
    /// A directory holding untracked or ignored content where the
    /// transition writes a file.
    DirectoryInTheWay,
    /// A file or symlink where the transition needs a directory.
    InTheWayOfDirectory,
    /// A tracked file with an uncommitted edit inside a directory the
    /// transition replaces with a file.
    ModifiedInReplacedDirectory,
}

impl OverlapKind {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            OverlapKind::Modified => "modified locally",
            OverlapKind::ModifiedRemovedByUpdate => "modified locally, removed by the update",
            OverlapKind::Deleted => "deleted locally",
            OverlapKind::Untracked => "untracked file in the way",
            OverlapKind::Ignored => "ignored file in the way",
            OverlapKind::CaseTwin => {
                "untracked file whose name differs only in case or Unicode form from one the update creates"
            }
            OverlapKind::NotAFile => "not a regular file",
            OverlapKind::DirectoryInTheWay => {
                "directory with untracked or ignored content where the update writes a file"
            }
            OverlapKind::InTheWayOfDirectory => {
                "file or symlink where the update creates a directory"
            }
            OverlapKind::ModifiedInReplacedDirectory => {
                "modified locally, in a directory the update replaces with a file"
            }
        }
    }

    /// A commit does not resolve it: it has to be moved aside by hand.
    fn in_the_way(self) -> bool {
        match self {
            OverlapKind::Modified
            | OverlapKind::ModifiedRemovedByUpdate
            | OverlapKind::Deleted
            | OverlapKind::Untracked
            | OverlapKind::ModifiedInReplacedDirectory => false,
            OverlapKind::Ignored
            | OverlapKind::CaseTwin
            | OverlapKind::NotAFile
            | OverlapKind::DirectoryInTheWay
            | OverlapKind::InTheWayOfDirectory => true,
        }
    }

    /// The materializer cannot write through it whatever the policy, so it
    /// refuses even a transition that overwrites uncommitted work.
    fn obstructs(self) -> bool {
        match self {
            OverlapKind::DirectoryInTheWay
            | OverlapKind::InTheWayOfDirectory
            | OverlapKind::ModifiedInReplacedDirectory => true,
            OverlapKind::Modified
            | OverlapKind::ModifiedRemovedByUpdate
            | OverlapKind::Deleted
            | OverlapKind::Untracked
            | OverlapKind::Ignored
            | OverlapKind::CaseTwin
            | OverlapKind::NotAFile => false,
        }
    }
}

/// A path the transition writes that holds content of neither snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Overlap {
    pub path: String,
    pub kind: OverlapKind,
}

/// A working-tree transition base -> target, classified against the disk.
///
/// Every path the two snapshots disagree on falls in exactly one set. Paths
/// they agree on are not listed and are never touched, which is what carries
/// an uncommitted edit to such a file across the transition.
#[derive(Debug, Default)]
pub(crate) struct Transition {
    /// Disk holds the old snapshot's content: written (or removed).
    pub update: Vec<String>,
    /// Disk already holds the new snapshot's content: left as it is.
    pub keep: Vec<String>,
    /// Disk holds neither. Never resolved silently: see [`OnOverlap`].
    pub overlap: Vec<Overlap>,
    /// What the record lists per path.
    changes: Vec<PathChange>,
}

/// How many overlapping paths a refusal names.
const OVERLAP_PATHS_SHOWN: usize = 20;

/// The refusal for a transition that would overwrite or remove the user's
/// work at `overlap`. Raised as [`oak_core::WorktreeTransition::Other`]; the
/// command that knows what it is names it
/// ([`OakError::in_transition`]).
fn refusal(overlap: &[&Overlap]) -> OakError {
    let mut overlap = overlap.to_vec();
    overlap.sort_by(|a, b| a.path.cmp(&b.path));
    OakError::LocalChangesWouldBeOverwritten {
        command: crate::output::invoked_command()
            .unwrap_or_else(|| format!("oak {}", operation_name())),
        clean_command: crate::output::invoked_command_with_flag("--clean"),
        transition: oak_core::WorktreeTransition::Other,
        paths: overlap
            .iter()
            .take(OVERLAP_PATHS_SHOWN)
            .map(|o| (o.path.clone(), o.kind.describe().to_string()))
            .collect(),
        total: overlap.len(),
        in_the_way: overlap.iter().any(|o| o.kind.in_the_way()),
    }
}

impl Transition {
    /// `Err` when the user's work is in the way under `policy`: any overlap
    /// when refusing, and an obstruction (which nothing writes through)
    /// always.
    pub(crate) fn refuse_under(&self, policy: OnOverlap) -> Result<()> {
        let blocking: Vec<&Overlap> = self
            .overlap
            .iter()
            .filter(|o| policy == OnOverlap::Refuse || o.kind.obstructs())
            .collect();
        if blocking.is_empty() {
            Ok(())
        } else {
            Err(refusal(&blocking))
        }
    }
}

/// Classify a transition and refuse it when it would overwrite uncommitted
/// work, without writing anything: for a caller whose own journal covers the
/// writes ([`covered`]), which must call this before that journal exists.
pub(crate) fn preflight(repo: &dyn Repository, root: &Path, plan: &Plan<'_>) -> Result<Transition> {
    let transition = classify(repo, root, plan)?;
    transition.refuse_under(plan.on_overlap)?;
    Ok(transition)
}

/// Refuse a whole-tree rewrite to `target` (one that overwrites uncommitted
/// files by request) when a file/directory obstruction stands in its way,
/// before anything is recorded or written: the materializer would stop
/// there part-way.
pub(crate) fn refuse_obstructions(
    repo: &dyn Repository,
    root: &Path,
    target: &Manifest,
) -> Result<()> {
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(oak_core::MetadataKey::SparsePaths)?
            .as_deref(),
    );
    let in_cone = |path: &str| cone.as_ref().is_none_or(|c| c.covers(path));
    let overlap: Vec<Overlap> =
        crate::materialize::transition_obstructions(root, repo, target, &in_cone, None)?
            .into_iter()
            .map(|obstruction| Overlap {
                kind: obstruction_kind(&obstruction),
                path: obstruction.path().to_string(),
            })
            .collect();
    if overlap.is_empty() {
        Ok(())
    } else {
        Err(refusal(&overlap.iter().collect::<Vec<_>>()))
    }
}

fn obstruction_kind(obstruction: &crate::materialize::Obstruction) -> OverlapKind {
    use crate::materialize::Obstruction;
    match obstruction {
        Obstruction::Directory(_) => OverlapKind::DirectoryInTheWay,
        Obstruction::ModifiedInDirectory(_) => OverlapKind::ModifiedInReplacedDirectory,
        Obstruction::NotADirectory(_) => OverlapKind::InTheWayOfDirectory,
    }
}

/// The one planner: sort every path a transition writes by what the disk
/// holds there — the old snapshot's content (`update`), already the new
/// one's (`keep`), or neither (`overlap`). One hash per path the snapshots
/// disagree on; paths they agree on are not read.
///
/// With [`OnOverlap::Overwrite`] and `capture_unknown`, the content of an
/// overlapping file is stored so a rollback can put it back.
pub(crate) fn classify(repo: &dyn Repository, root: &Path, plan: &Plan<'_>) -> Result<Transition> {
    let mut out = Transition::default();
    let capture = plan.capture_unknown && plan.on_overlap == OnOverlap::Overwrite;
    let cone = match plan.sparse_paths_before {
        Some(_) => None,
        None => oak_core::SparseCone::from_metadata(
            repo.get_metadata(oak_core::MetadataKey::SparsePaths)?
                .as_deref(),
        ),
    };
    let in_cone = |path: &str| cone.as_ref().is_none_or(|c| c.covers(path));
    let replaced: HashMap<&str, &ManifestEntry> = plan
        .replaced
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    // Content the server withheld, that was declared lost, or that is not
    // in local storage (not hydrated yet) was never written here: a tracked
    // file absent for that reason is not a deletion.
    let withheld = crate::commands::restricted::load_restricted_blobs(repo);
    let known_lost = crate::commands::known_loss::load_known_lost_blobs(repo);
    let never_written = |hash: &Hash| {
        withheld.contains(hash.as_str())
            || known_lost.contains(hash.as_str())
            || !repo.has_blob(hash).unwrap_or(true)
    };
    let disk = Disk::new(root, repo);
    // Read only when an untracked file is in the way: an ignored one is
    // reported as such, since a commit does not pick it up.
    let mut ignore: Option<oak_core::IgnorePatterns> = None;
    let mut is_ignored = |path: &str| -> Result<bool> {
        if ignore.is_none() {
            ignore = Some(oak_core::IgnorePatterns::new(root)?);
        }
        Ok(ignore
            .as_ref()
            .is_some_and(|ignore| ignore.is_ignored(Path::new(path), false)))
    };
    let dropped_paths: HashSet<&str> = plan.dropped.iter().map(|e| e.path.as_str()).collect();
    for entry in plan.target.entries.iter().filter(|e| in_cone(&e.path)) {
        let after = Side::File {
            hash: entry.blob_hash.clone(),
            mode: entry.mode,
        };
        let old = replaced.get(entry.path.as_str());
        let committed = old.map(|old| Side::File {
            hash: old.blob_hash.clone(),
            mode: old.mode,
        });
        let mut overlap = None;
        // Where the overlap is listed, when not at `entry.path`.
        let mut overlap_at = None;
        let before = match fs::symlink_metadata(root.join(&entry.path)) {
            // A directory here is replaced by the file. Its tracked files
            // are among the dropped paths; anything else inside is an
            // obstruction, found by the pass below.
            Ok(md) if md.is_dir() => Side::Missing,
            Err(_) => {
                // A tracked file that is gone was deleted locally — unless
                // it was never written (withheld), or sits under another
                // spelling (then it is one of the dropped paths).
                if let Some(old) = old {
                    if !plan.replaced_is_disk
                        && !never_written(&old.blob_hash)
                        && disk.actual_spelling(&entry.path).is_none()
                    {
                        overlap = Some(OverlapKind::Deleted);
                    }
                }
                Side::Missing
            }
            Ok(md) => match (old, committed) {
                (Some(old), Some(committed)) => {
                    // What is on disk is read, not assumed: one hash per
                    // file the update is about to overwrite (which it
                    // reads again anyway to skip files already right).
                    // (Read directly: a path both snapshots track is
                    // spelled the same in both, so no listing is needed.)
                    let on_disk = (!plan.replaced_is_disk)
                        .then(|| entry_mode(&md))
                        .flatten()
                        .and_then(|mode| {
                            let bytes = read_entry(&root.join(&entry.path), mode).ok()?;
                            Some((repo.hash_blob_content(&bytes), mode, bytes))
                        });
                    match on_disk {
                        // A whole-tree capture: `replaced` is the disk.
                        None if plan.replaced_is_disk => committed,
                        Some((hash, mode, _)) if is_entry(&hash, mode, old) => committed,
                        // Already the incoming content: recorded as
                        // unchanged, so nothing is undone there.
                        Some((hash, mode, _)) if is_entry(&hash, mode, entry) => {
                            Side::File { hash, mode }
                        }
                        // An uncommitted edit.
                        Some((hash, mode, bytes)) => {
                            overlap = Some(OverlapKind::Modified);
                            if capture {
                                // Stored, so a rollback puts back the edit
                                // and not the committed file.
                                repo.put_blob(bytes)?;
                                Side::File { hash, mode }
                            } else {
                                committed
                            }
                        }
                        // Not a file or a symlink, or unreadable.
                        None => {
                            overlap = Some(OverlapKind::NotAFile);
                            committed
                        }
                    }
                }
                // Rare, so worth a closer look: a file the old snapshot
                // does not have at this path. Under another spelling (a
                // case-only rename on a case-insensitive filesystem) it
                // is one of the dropped paths, and this spelling is new.
                // Otherwise it is the user's untracked file.
                _ => match disk.probe(&entry.path) {
                    // Reached (the lstat above found it) but not listed
                    // under this spelling: a name differing only in case,
                    // which the filesystem treats as the same file (fb-656).
                    // Writing this path would write into it. Only a tracked
                    // file the update drops there (a case-only rename) is
                    // the update's to replace.
                    Probe::Missing => match disk.actual_spelling(&entry.path) {
                        Some(actual)
                            if !dropped_paths.contains(actual.as_str())
                                && !tracked_at_head(repo, &actual)? =>
                        {
                            overlap = Some(OverlapKind::CaseTwin);
                            let side = match (capture, entry_mode(&md)) {
                                (true, Some(mode)) => {
                                    let bytes = read_entry(&root.join(&entry.path), mode)?;
                                    let hash = repo.hash_blob_content(&bytes);
                                    repo.put_blob(bytes)?;
                                    Side::File { hash, mode }
                                }
                                _ => Side::Unknown,
                            };
                            overlap_at = Some(actual);
                            side
                        }
                        _ => Side::Missing,
                    },
                    Probe::Directory => Side::Missing,
                    Probe::File { hash, mode } if is_entry(&hash, mode, entry) => {
                        Side::File { hash, mode }
                    }
                    Probe::File { hash, mode } => {
                        overlap = Some(if is_ignored(&entry.path)? {
                            OverlapKind::Ignored
                        } else {
                            OverlapKind::Untracked
                        });
                        if capture {
                            repo.put_blob(read_entry(&root.join(&entry.path), mode)?)?;
                            Side::File { hash, mode }
                        } else {
                            Side::Unknown
                        }
                    }
                    Probe::Other => {
                        overlap = Some(OverlapKind::NotAFile);
                        Side::Unknown
                    }
                },
            },
        };
        match overlap {
            Some(kind) => out.overlap.push(Overlap {
                path: overlap_at.unwrap_or_else(|| entry.path.clone()),
                kind,
            }),
            None if before == after => out.keep.push(entry.path.clone()),
            None => out.update.push(entry.path.clone()),
        }
        out.changes.push(PathChange {
            path: entry.path.clone(),
            before,
            after,
        });
    }
    for entry in plan.dropped.iter().filter(|e| in_cone(&e.path)) {
        // By exact spelling: on a case-insensitive filesystem the old side
        // of a case-only rename reaches the file that now carries the new
        // spelling, which is the target's.
        let on_disk = if plan.replaced_is_disk {
            fs::symlink_metadata(root.join(&entry.path))
                .is_ok_and(|md| !md.is_dir())
                .then_some(None)
        } else {
            match disk.probe(&entry.path) {
                // A tracked file the user had already deleted is not this
                // update's to remove, or to restore.
                Probe::Missing | Probe::Directory => None,
                Probe::File { hash, mode } => Some(Some((hash, mode))),
                Probe::Other => Some(None),
            }
        };
        let Some(on_disk) = on_disk else {
            out.keep.push(entry.path.clone());
            continue;
        };
        match on_disk {
            Some((hash, mode)) if !is_entry(&hash, mode, entry) => out.overlap.push(Overlap {
                path: entry.path.clone(),
                kind: OverlapKind::ModifiedRemovedByUpdate,
            }),
            None if !plan.replaced_is_disk => out.overlap.push(Overlap {
                path: entry.path.clone(),
                kind: OverlapKind::NotAFile,
            }),
            _ => out.update.push(entry.path.clone()),
        }
        out.changes.push(PathChange {
            path: entry.path.clone(),
            before: Side::File {
                hash: entry.blob_hash.clone(),
                mode: entry.mode,
            },
            after: Side::Missing,
        });
    }
    // File/directory obstructions (fb-657): something of the user's where
    // the target needs the other kind of entry, or under a path the target
    // needs as a directory. The materializer's own check, run here so the
    // refusal comes before any record or journal exists. A whole-tree
    // capture's `dropped` is the disk itself, not tracked files: there only
    // HEAD's files are replaceable, as in the materializer.
    let old = (!plan.replaced_is_disk).then_some(plan.dropped);
    for obstruction in
        crate::materialize::transition_obstructions(root, repo, plan.target, &in_cone, old)?
    {
        let kind = obstruction_kind(&obstruction);
        match out
            .overlap
            .iter_mut()
            .find(|o| o.path == obstruction.path())
        {
            Some(existing) => existing.kind = kind,
            None => out.overlap.push(Overlap {
                path: obstruction.path().to_string(),
                kind,
            }),
        }
    }
    Ok(out)
}

/// Whether HEAD's snapshot tracks `path` (a file).
fn tracked_at_head(repo: &dyn Repository, path: &str) -> Result<bool> {
    let Some(head) = repo.get_head()? else {
        return Ok(false);
    };
    let Some(commit) = repo.get_commit(&head)? else {
        return Ok(false);
    };
    Ok(repo
        .resolve_path_in_tree(&commit.manifest_hash, path)?
        .is_some_and(|entry| entry.kind != oak_core::TreeEntryKind::Tree))
}

/// Whether content with `hash` and `mode` on disk is exactly `entry`.
fn is_entry(hash: &Hash, mode: FileMode, entry: &ManifestEntry) -> bool {
    // Off unix there is no executable bit to compare.
    *hash == entry.blob_hash && (mode == entry.mode || !cfg!(unix))
}

/// Proof that a working-tree update was recorded. The record outlives the
/// file writes on purpose: call [`Self::finish`] only after the refs (or
/// conflict state) that make the new tree the committed one are stored. An
/// error path that drops it leaves the record, and recovery puts the tree
/// back.
#[must_use = "finish() the update after publishing its refs, or the next command is refused"]
pub(crate) struct WorktreeUpdate {
    file: Option<PathBuf>,
}

impl WorktreeUpdate {
    /// An update with nothing to record (nothing written, or covered by an
    /// enclosing record).
    pub(crate) fn none() -> Self {
        Self { file: None }
    }

    pub(crate) fn finish(self) {
        if let Some(file) = self.file {
            let _ = fs::remove_file(file);
        }
    }

    /// The update failed in this process: recover now, while the lock is
    /// still held, instead of leaving it for the next command. If that fails
    /// too the record stays.
    pub(crate) fn abandon(self, lock: &WorkdirLock, root: &Path, repo: &dyn Repository) {
        let Some(file) = self.file else { return };
        let Some(oak_dir) = file.parent() else { return };
        let _ = recover(lock, oak_dir, root, repo, false);
    }
}

/// Where a materialization writes a file before renaming it into place.
///
/// Normally the staging directory under `.oak/`, so a writer killed mid-file
/// leaves nothing in the working tree. Where a rename out of it is not
/// possible (the destination is on another filesystem), the temp file has to
/// be a sibling of its destination. Such a sibling is named after the
/// update's record, and the record is marked before the first one is made,
/// so recovery can remove exactly the files this update left and never a
/// file of the user's that merely looks like one.
pub(crate) struct Staging {
    dir: Option<PathBuf>,
    /// The state directory and id of the record covering these writes.
    record: Option<(PathBuf, String)>,
    marked: std::cell::Cell<bool>,
}

impl Staging {
    pub(crate) fn for_update(lock: &WorkdirLock) -> Self {
        let oak_dir = lock.oak_dir();
        let dir = Some(oak_dir.join(TEMP_DIR))
            .filter(|_| !staging_disabled_for_test())
            .filter(|dir| fs::create_dir_all(dir).is_ok());
        let record = match read(oak_dir) {
            Ok(Some(Record::Readable(journal))) if !journal.id.is_empty() => {
                Some((oak_dir.to_path_buf(), journal.id.clone()))
            }
            _ => None,
        };
        Self {
            dir,
            record,
            marked: std::cell::Cell::new(false),
        }
    }

    /// A fresh path in the staging directory, when there is one.
    pub(crate) fn staged(&self) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!(".oak.tmp-{}", uuid::Uuid::new_v4())))
    }

    /// A fresh temp path next to `path`, for when staging is not possible.
    pub(crate) fn sibling(&self, path: &Path) -> Result<PathBuf> {
        let Some((oak_dir, id)) = &self.record else {
            return crate::atomic_file::temp_sibling(path);
        };
        if !self.marked.replace(true) {
            mark_sibling_temps(oak_dir)?;
        }
        Ok(path.with_file_name(format!(
            "{}{}",
            sibling_prefix(id),
            uuid::Uuid::new_v4().simple()
        )))
    }
}

fn sibling_prefix(id: &str) -> String {
    format!(".oak.tmp-{id}-")
}

/// Debug builds only: `OAK_TEST_NO_STAGING=1` makes every write use the
/// sibling fallback, as on a filesystem that cannot rename out of `.oak/`.
fn staging_disabled_for_test() -> bool {
    cfg!(debug_assertions) && std::env::var_os("OAK_TEST_NO_STAGING").is_some()
}

fn mark_sibling_temps(oak_dir: &Path) -> Result<()> {
    let file = oak_dir.join(FILE);
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
    if record["sibling_temps"] == true {
        return Ok(());
    }
    record["sibling_temps"] = serde_json::Value::Bool(true);
    write_record(&file, &serde_json::to_vec(&record)?)
}

/// The `oak` subcommand this process is running, for the record and the
/// messages built from it. Flags are left out: they can carry secrets.
fn operation_name() -> String {
    std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .filter(|arg| {
            arg.len() <= 32
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .unwrap_or_else(|| "command".to_string())
}

fn read(oak_dir: &Path) -> Result<Option<Record>> {
    let bytes = match fs::read(oak_dir.join(FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // A record that cannot be read still means "an update was interrupted":
    // never treat it as absent, and never as something safe to clear.
    Ok(Some(match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) if journal.version == VERSION => Record::Readable(Box::new(journal)),
        Ok(journal) if journal.version > VERSION => Record::Unusable(Some(journal.version)),
        _ => match serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("version")?.as_u64())
        {
            Some(version) if version > u64::from(VERSION) => Record::Unusable(Some(version as u32)),
            _ => Record::Unusable(None),
        },
    }))
}

/// True when a record (of any kind) exists, or an interrupted switch's
/// journal does: for commands that may run but must not rewrite the tree.
pub(crate) fn recorded(oak_dir: &Path) -> bool {
    oak_dir.join(FILE).exists()
}

/// True when an enclosing record already covers working-tree writes made
/// under `lock`: this process's own, or an interrupted-switch journal.
pub(crate) fn covered(lock: &WorkdirLock) -> bool {
    let oak_dir = lock.oak_dir();
    oak_dir.join("switch-journal.json").exists()
        || matches!(read(oak_dir), Ok(Some(Record::Readable(journal))) if journal.pid == std::process::id())
}

/// Record an update before its first write. Returns an inert handle when an
/// enclosing record already covers it ([`covered`]).
pub(crate) fn begin(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    recovery: Recovery,
    plan: Option<Plan<'_>>,
) -> Result<WorktreeUpdate> {
    let oak_dir = lock.oak_dir();
    if covered(lock) {
        return Ok(WorktreeUpdate::none());
    }
    if let Some(existing) = read(oak_dir)? {
        return Err(interrupted_error(oak_dir, &existing));
    }

    let mut paths = Vec::new();
    let mut sparse_paths = None;
    let mut refs_after = None;
    if let Some(plan) = plan {
        // Classified, and refused when uncommitted work is in the way,
        // before the record exists and before the first write. The record
        // lists exactly the classified plan.
        paths = preflight(repo, root, &plan)?.changes;
        refs_after = plan.refs_after;
        sparse_paths = plan.sparse_paths_before;
    }

    let refs = Refs::current(repo)?;
    let journal = Journal {
        version: VERSION,
        operation: operation_name(),
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
        branch: refs.branch,
        head: refs.head,
        branch_head: refs.branch_head,
        refs_after,
        recovery,
        sparse_paths,
        paths,
        id: uuid::Uuid::new_v4().simple().to_string(),
        sibling_temps: false,
    };
    let file = oak_dir.join(FILE);
    write_record(&file, &serde_json::to_vec(&journal)?)?;
    Ok(WorktreeUpdate { file: Some(file) })
}

/// The tracked mode of what `md` (an lstat) describes; `None` for anything
/// that is not a file or a symlink.
fn entry_mode(md: &fs::Metadata) -> Option<FileMode> {
    if md.file_type().is_symlink() {
        return Some(FileMode::Symlink);
    }
    if !md.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o111 != 0 {
            return Some(FileMode::Executable);
        }
    }
    Some(FileMode::Regular)
}

fn read_entry(full: &Path, mode: FileMode) -> Result<Vec<u8>> {
    Ok(if mode == FileMode::Symlink {
        fs::read_link(full)?.as_os_str().as_encoded_bytes().to_vec()
    } else {
        fs::read(full)?
    })
}

/// One record per update, written before any file of it, and atomically: a
/// temp sibling, `fsync`, rename, `fsync` of the directory. A crash while
/// writing leaves no record or the whole one, never part of one. Plain
/// `fsync` rather than `sync_all` (`F_FULLFSYNC` on macOS, a drive-cache
/// flush): the working-tree writes that follow are not flushed either, so
/// ordering against them is what matters, and that costs one inode sync per
/// update instead of a device flush.
fn write_record(file: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = crate::atomic_file::temp_sibling(file)?;
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&tmp)?;
        handle.write_all(bytes)?;
        sync_inode(&handle)?;
        drop(handle);
        fs::rename(&tmp, file)?;
        #[cfg(unix)]
        if let Some(parent) = file.parent() {
            sync_inode(&fs::File::open(parent)?)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn sync_inode(file: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(unix))]
    file.sync_all()?;
    Ok(())
}

/// An interrupted update, as reported to the user.
#[derive(Debug, Clone)]
pub struct Pending {
    pub operation: String,
    pub started_at: String,
    /// What happened, what recovery does, and what it does not bring back.
    pub message: String,
    /// The non-destructive recovery first.
    pub next_commands: Vec<String>,
}

impl Pending {
    fn of(oak_dir: &Path, record: &Record) -> Self {
        match record {
            Record::Unusable(Some(version)) => Pending {
                operation: "unknown".to_string(),
                started_at: String::new(),
                message: format!(
                    "A working-tree update was interrupted, and its record (.oak/{FILE}) was \
                     written by a newer oak (format version {version}). This oak cannot recover \
                     it and will not discard it. Upgrade oak (`oak upgrade`), then run \
                     `oak pull --abort`."
                ),
                next_commands: vec!["oak upgrade".to_string()],
            },
            Record::Unusable(None) => Pending {
                operation: "unknown".to_string(),
                started_at: String::new(),
                message: format!(
                    "A working-tree update was interrupted, but its record (.oak/{FILE}) cannot \
                     be read, so Oak cannot tell which files it had written and cannot recover \
                     automatically. Keep the record and the working tree, and compare the tree \
                     with the branch using `oak diff --json`. Only when the tree holds what you \
                     want, delete .oak/{FILE} by hand: its contents then count as ordinary \
                     uncommitted changes."
                ),
                next_commands: vec!["oak diff --json".to_string()],
            },
            Record::Readable(journal) => {
                let operation = &journal.operation;
                // `oak pull --abort` only recovers. `oak pull` recovers and
                // then pulls again, which rewrites the incoming paths, so it
                // comes second, and only where a pull was what was running
                // and no paused merge or sync would stop it.
                let mut next_commands = vec!["oak pull --abort".to_string()];
                if matches!(operation.as_str(), "pull" | "watch")
                    && !conflict_state_present(oak_dir)
                {
                    next_commands.push("oak pull".to_string());
                }
                let effect = match journal.recovery {
                    Recovery::Rollback => format!(
                        "It puts the files `oak {operation}` had written back to what they held \
                         before and removes the ones it had added. Content it did not write \
                         (your edits, untracked files, anything changed since) is left in place \
                         and listed. An uncommitted edit or untracked file that \
                         `oak {operation}` had already overwritten is put back as it was."
                    ),
                    Recovery::Complete => format!(
                        "The branch already points at the snapshot `oak {operation}` was \
                         writing, so it writes the remaining files. Content the update did not \
                         know (edits made since) is left in place and listed."
                    ),
                    Recovery::Clear => format!(
                        "It only clears this state: files `oak {operation}` had already \
                         rewritten stay rewritten, the rest keep your uncommitted changes. Rerun \
                         `oak {operation}` afterwards to finish it."
                    ),
                };
                Pending {
                    operation: operation.clone(),
                    started_at: journal.started_at.clone(),
                    message: format!(
                        "An interrupted `oak {operation}` left the working tree partially \
                         updated. Run `oak pull --abort` before modifying or recording it. {effect}"
                    ),
                    next_commands,
                }
            }
        }
    }

    fn into_error(self) -> OakError {
        OakError::InterruptedWorktreeUpdate {
            message: self.message,
            next_commands: self.next_commands,
        }
    }
}

fn conflict_state_present(oak_dir: &Path) -> bool {
    ["SYNC_HEAD", "MERGE_HEAD"]
        .iter()
        .any(|name| oak_dir.join(name).exists())
}

fn interrupted_error(oak_dir: &Path, record: &Record) -> OakError {
    Pending::of(oak_dir, record).into_error()
}

/// The interrupted update recorded in `oak_dir`, if any. A record whose
/// writer is alive and still holds the working-directory lock is an update in
/// progress, not an interrupted one.
pub fn pending(oak_dir: &Path) -> Option<Pending> {
    let record = read(oak_dir).ok()??;
    if let Record::Readable(journal) = &record {
        if crate::workdir_lock::live_holder(oak_dir) == Some(journal.pid) {
            return None;
        }
    }
    Some(Pending::of(oak_dir, &record))
}

/// Refuse, before mutating anything, when an interrupted update is recorded:
/// for multi-step commands (`oak finish`) whose first steps take no lock.
pub(crate) fn refuse_if_pending(oak_dir: &Path) -> Result<()> {
    match pending(oak_dir) {
        Some(pending) => Err(pending.into_error()),
        None => Ok(()),
    }
}

/// Refuse while an interrupted update is recorded. Called with the
/// working-directory lock held, so any record is a dead writer's.
pub(crate) fn ensure_none(oak_dir: &Path) -> Result<()> {
    match read(oak_dir)? {
        Some(record) => Err(interrupted_error(oak_dir, &record)),
        None => Ok(()),
    }
}

/// A path recovery left exactly as it found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preserved {
    pub path: String,
    pub reason: &'static str,
}

/// What [`recover_at`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recovered {
    pub operation: String,
    /// `rolled_back`, `completed`, or `cleared`.
    pub action: &'static str,
    /// The refs had moved (or a paused merge/sync was recorded) since the
    /// update began, so nothing was replayed.
    pub superseded: bool,
    /// Paths written: their pre-update content (`rolled_back`) or the
    /// update's (`completed`).
    pub written: Vec<String>,
    /// Paths removed: files the update had added (`rolled_back`) or was
    /// dropping (`completed`).
    pub removed: Vec<String>,
    /// Paths holding content that is neither side of the update: left alone.
    pub preserved: Vec<Preserved>,
    pub temp_files_removed: usize,
}

impl Recovered {
    pub fn message(&self) -> String {
        let what = match (self.action, self.superseded) {
            (_, true) => {
                "the branch had moved on since, so no file was changed and only its record \
                 was cleared"
                    .to_string()
            }
            ("rolled_back", _) => format!(
                "put {} file(s) back to their content from before it and removed {} it had added",
                self.written.len(),
                self.removed.len()
            ),
            ("completed", _) => format!(
                "wrote the {} file(s) it had not reached and removed {} it was dropping",
                self.written.len(),
                self.removed.len()
            ),
            _ => "cleared its record; files it had already rewritten were kept as they are"
                .to_string(),
        };
        let mut message = format!("Recovered an interrupted `oak {}`: {what}", self.operation);
        if !self.preserved.is_empty() {
            message.push_str(&format!(
                ". Left {} path(s) untouched because they hold content the update did not \
                 write — check them:",
                self.preserved.len()
            ));
            for kept in &self.preserved {
                message.push_str(&format!("\n  {} ({})", kept.path, kept.reason));
            }
        }
        message
    }
}

/// Recover the checkout at `path` from an interrupted update, if one is
/// recorded. Re-entrant: every step re-reads the disk and the record is
/// removed last, so a recovery that is itself killed is recovered by the
/// next one.
pub fn recover_at(path: &Path) -> Result<Option<Recovered>> {
    recover_checkout(path, false)
}

/// [`recover_at`] for a caller no one is watching (the watch daemon): only a
/// recovery that leaves nothing for a person to decide. A record that merely
/// clears, or one with paths that would be left untouched, stays — along
/// with the refusal it causes — and is returned as the error.
pub fn recover_unattended(path: &Path) -> Result<Option<Recovered>> {
    recover_checkout(path, true)
}

fn recover_checkout(path: &Path, unattended: bool) -> Result<Option<Recovered>> {
    let Ok(ctx) = crate::resolve::resolve(path) else {
        return Ok(None);
    };
    if !recorded(&ctx.oak_dir) {
        return Ok(None);
    }
    let lock = WorkdirLock::acquire_for_update_recovery(&ctx.oak_dir)?;
    let repo = ctx.open()?;
    recover(
        &lock,
        &ctx.oak_dir,
        &ctx.work_tree,
        repo.as_ref(),
        unattended,
    )
}

/// The blobs recovery at `path` will write that are not in local storage:
/// what a caller with network access can fetch before recovering.
pub fn blobs_needed(path: &Path) -> Vec<Hash> {
    let Ok(ctx) = crate::resolve::resolve(path) else {
        return Vec::new();
    };
    let Ok(Some(Record::Readable(journal))) = read(&ctx.oak_dir) else {
        return Vec::new();
    };
    let Ok(repo) = ctx.open() else {
        return Vec::new();
    };
    let mut needed: Vec<Hash> = Vec::new();
    for change in &journal.paths {
        let wanted = match journal.recovery {
            Recovery::Rollback => &change.before,
            Recovery::Complete => &change.after,
            Recovery::Clear => continue,
        };
        if let Side::File { hash, .. } = wanted {
            if !needed.contains(hash) && !repo.has_blob(hash).unwrap_or(true) {
                needed.push(hash.clone());
            }
        }
    }
    needed
}

fn recover(
    lock: &WorkdirLock,
    oak_dir: &Path,
    root: &Path,
    repo: &dyn Repository,
    unattended: bool,
) -> Result<Option<Recovered>> {
    let journal = match read(oak_dir)? {
        None => return Ok(None),
        Some(Record::Readable(journal)) => journal,
        // Nothing to act on, and clearing it would hand a torn tree to the
        // next commit: stay blocked and say so.
        Some(record) => return Err(interrupted_error(oak_dir, &record)),
    };
    // Refs that moved, or conflict state written after the tree, mean the
    // operation got past its file writes (or another binary carried on):
    // the tree on disk is the one those refs describe.
    let now = Refs::current(repo)?;
    let at_before = now
        == Refs {
            branch: journal.branch.clone(),
            head: journal.head.clone(),
            branch_head: journal.branch_head.clone(),
        };
    let at_after = journal.refs_after.as_ref() == Some(&now);
    let paused = conflict_state_present(oak_dir) || conflict_state_present(&root.join(".oak"));
    let direction = match journal.recovery {
        _ if paused => None,
        Recovery::Clear => None,
        Recovery::Rollback if at_before => Some(Direction::Back),
        // Refs-first: forward once they have moved; if the process died
        // before moving them, the old snapshot is still the committed one.
        Recovery::Complete if at_after => Some(Direction::Forward),
        Recovery::Complete if at_before && journal.refs_after.is_some() => Some(Direction::Back),
        Recovery::Complete if at_before => Some(Direction::Forward),
        _ => None,
    };
    let superseded = paused || !(at_before || at_after);
    if unattended && direction.is_none() && !superseded {
        return Err(OakError::InterruptedWorktreeUpdate {
            message: format!(
                "{} (Not recovered automatically: clearing it is a decision about your \
                 uncommitted changes.)",
                Pending::of(oak_dir, &Record::Readable(journal)).message
            ),
            next_commands: vec!["oak pull --abort".to_string()],
        });
    }

    let mut report = Recovered {
        operation: journal.operation.clone(),
        action: match direction {
            Some(Direction::Back) => "rolled_back",
            Some(Direction::Forward) => "completed",
            None => "cleared",
        },
        superseded,
        written: Vec::new(),
        removed: Vec::new(),
        preserved: Vec::new(),
        temp_files_removed: 0,
    };
    if let Some(direction) = direction {
        replay_journal(
            lock,
            root,
            repo,
            &journal,
            direction,
            unattended,
            &mut report,
        )?;
    }
    report.temp_files_removed = remove_temp_files(oak_dir, root, &journal);
    match fs::remove_file(oak_dir.join(FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Some(report))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Toward what was there before the update.
    Back,
    /// Toward what the update writes.
    Forward,
}

/// What is at a path, by its exact spelling and without following symlinks.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Missing,
    Directory,
    File {
        hash: Hash,
        mode: FileMode,
    },
    /// Neither a file, a symlink, nor a directory, or unreadable.
    Other,
}

/// Reads the working tree for recovery. A path counts as present only under
/// its exact spelling: on a case-insensitive filesystem `Case.txt` and
/// `case.txt` reach the same file, and a case-only rename is exactly the
/// difference between the two sides of an update. Directory listings are
/// cached per pass.
struct Disk<'a> {
    root: &'a Path,
    repo: &'a dyn Repository,
    listings: std::cell::RefCell<HashMap<PathBuf, HashSet<OsString>>>,
}

impl<'a> Disk<'a> {
    fn new(root: &'a Path, repo: &'a dyn Repository) -> Self {
        Self {
            root,
            repo,
            listings: Default::default(),
        }
    }

    fn lists(&self, dir: &Path, name: &str) -> bool {
        let mut listings = self.listings.borrow_mut();
        listings
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                fs::read_dir(dir)
                    .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
                    .unwrap_or_default()
            })
            .contains(std::ffi::OsStr::new(name))
    }

    /// How `path` is spelled on disk when it is reachable only under another
    /// spelling: `None` when it is present as spelled or not reachable at all.
    ///
    /// Which spellings name one entry is the filesystem's decision, not ours:
    /// APFS and HFS+ fold case *and* Unicode normalization (an NFD `café`
    /// is the same file as an NFC one), other filesystems fold neither. So
    /// instead of guessing a folding rule, ask the filesystem whether the
    /// spelling resolves at all, and if it does, find the listed entry it
    /// resolves to by identity (same device and inode).
    fn actual_spelling(&self, path: &str) -> Option<String> {
        let mut dir = self.root.to_path_buf();
        let mut actual = Vec::new();
        let mut differs = false;
        for component in path.split('/').filter(|c| !c.is_empty()) {
            let name = if self.lists(&dir, component) {
                component.to_string()
            } else {
                let spelled = dir.join(component);
                // Not reachable under this spelling: nothing is in the way.
                fs::symlink_metadata(&spelled).ok()?;
                let found = fs::read_dir(&dir)
                    .ok()?
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .find(|name| crate::materialize::same_entry(&dir.join(name), &spelled))?;
                differs = true;
                found
            };
            dir.push(&name);
            actual.push(name);
        }
        differs.then(|| actual.join("/"))
    }

    fn probe(&self, path: &str) -> Probe {
        let mut dir = self.root.to_path_buf();
        let mut components = path.split('/').filter(|c| !c.is_empty()).peekable();
        while let Some(component) = components.next() {
            if !self.lists(&dir, component) {
                return Probe::Missing;
            }
            dir.push(component);
            if components.peek().is_some() {
                // Only a real directory can hold the rest of the path.
                match fs::symlink_metadata(&dir) {
                    Ok(md) if md.is_dir() => {}
                    _ => return Probe::Missing,
                }
            }
        }
        let full = dir;
        let Ok(md) = fs::symlink_metadata(&full) else {
            return Probe::Missing;
        };
        if md.is_dir() {
            return Probe::Directory;
        }
        let Some(mode) = entry_mode(&md) else {
            return Probe::Other;
        };
        match read_entry(&full, mode) {
            Ok(bytes) => Probe::File {
                hash: self.repo.hash_blob_content(&bytes),
                mode,
            },
            Err(_) => Probe::Other,
        }
    }
}

fn side_is(probe: &Probe, side: &Side) -> bool {
    match (probe, side) {
        (Probe::Missing, Side::Missing) => true,
        (Probe::File { hash, mode }, Side::File { hash: h, mode: m }) => {
            // Off unix there is no executable bit to compare.
            hash == h && (mode == m || !cfg!(unix))
        }
        _ => false,
    }
}

fn entry(path: &str, hash: &Hash, mode: FileMode) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        blob_hash: hash.clone(),
        mode,
    }
}

const NOT_THE_UPDATES: &str =
    "holds content that is neither what the update replaced nor what it wrote";
const IN_THE_WAY: &str = "a directory with other content is in the way";
const BEFORE_UNKNOWN: &str =
    "the update's content was kept: what it replaced here was not recorded";

/// Move every recorded path that still holds one side of the update to the
/// side `direction` names. Removals run first and the disk is read again
/// before the writes, so a file the update replaced with a directory (or the
/// reverse) is seen as restorable once the other side's files are gone — on
/// the first run and on a rerun after a kill alike.
fn replay_journal(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    journal: &Journal,
    direction: Direction,
    strict: bool,
    report: &mut Recovered,
) -> Result<()> {
    // (path, from, to): `from` is the side being left, `to` the one wanted.
    let moves: Vec<(&str, &Side, &Side)> = journal
        .paths
        .iter()
        .filter(|change| change.before != change.after)
        .map(|change| match direction {
            Direction::Back => (change.path.as_str(), &change.after, &change.before),
            Direction::Forward => (change.path.as_str(), &change.before, &change.after),
        })
        .collect();
    // Content the server withheld is absent by design, not missing.
    let withheld = crate::commands::restricted::load_restricted_blobs(repo);

    let classify = |disk: &Disk<'_>, final_pass: bool| -> Result<Classified> {
        let mut out = Classified::default();
        for &(path, from, to) in &moves {
            let probe = disk.probe(path);
            if let (Probe::Missing, Side::Missing, Side::File { hash, mode }) = (&probe, to, from) {
                // Nothing under this spelling — but on a case-insensitive
                // filesystem the content written for it may sit under the
                // other side's spelling (a case-only rename, killed before
                // the name was changed). It goes unless that spelling wants
                // exactly this content.
                if let Some(actual) = disk.actual_spelling(path) {
                    let there = disk.probe(&actual);
                    let wanted_there = moves.iter().find(|m| m.0 == actual).map(|m| m.2);
                    if side_is(&there, from)
                        && !wanted_there.is_some_and(|wanted| side_is(&there, wanted))
                    {
                        out.remove.push(entry(path, hash, *mode));
                    }
                }
                continue;
            }
            // A directory where the wanted side has no file is that side.
            if side_is(&probe, to) || (*to == Side::Missing && probe == Probe::Directory) {
                continue;
            }
            match to {
                Side::Missing => {
                    if let (true, Side::File { hash, mode }) = (side_is(&probe, from), from) {
                        out.remove.push(entry(path, hash, *mode));
                    } else if probe != Probe::Missing {
                        out.keep(path, NOT_THE_UPDATES);
                    }
                }
                Side::File { hash, mode } => {
                    let replaceable = side_is(&probe, from)
                        // Forward only: the update overwrites what it never
                        // identified, exactly as it was about to.
                        || (*from == Side::Unknown && direction == Direction::Forward);
                    if probe == Probe::Directory && !final_pass {
                        // Decided once the removals have run.
                    } else if probe == Probe::Directory {
                        out.keep(path, IN_THE_WAY);
                    } else if !replaceable {
                        out.keep(path, NOT_THE_UPDATES);
                    } else if withheld.contains(hash.as_str()) {
                        // Never written here, before or after.
                    } else if !repo.has_blob(hash)? {
                        out.missing.push((hash.clone(), path.to_string()));
                    } else {
                        out.write.push(entry(path, hash, *mode));
                    }
                }
                Side::Unknown => {
                    if side_is(&probe, from) {
                        out.keep(path, BEFORE_UNKNOWN);
                    }
                }
            }
        }
        Ok(out)
    };
    let refuse = |found: &Classified| -> Result<()> {
        if !found.missing.is_empty() {
            let mut paths: Vec<&str> = found.missing.iter().map(|(_, p)| p.as_str()).collect();
            paths.sort();
            let shown = paths
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(OakError::InterruptedWorktreeUpdate {
                message: format!(
                    "Cannot recover the interrupted `oak {}`: the content to write for {} \
                     path(s) is not in local storage ({shown}{}). Nothing was changed and the \
                     record is kept. `oak pull --abort` fetches it from the remote when it can: \
                     run it again with network access.",
                    journal.operation,
                    paths.len(),
                    if paths.len() > 10 { ", ..." } else { "" }
                ),
                next_commands: vec!["oak pull --abort".to_string()],
            });
        }
        if strict && !found.preserved.is_empty() {
            return Err(OakError::InterruptedWorktreeUpdate {
                message: format!(
                    "The interrupted `oak {}` was not recovered automatically: {} path(s) hold \
                     content the update did not write (first: {}). Run `oak pull --abort` to \
                     recover; it leaves them in place and lists them.",
                    journal.operation,
                    found.preserved.len(),
                    found.preserved[0].path
                ),
                next_commands: vec!["oak pull --abort".to_string()],
            });
        }
        Ok(())
    };

    // Decide everything that can be decided before touching the tree.
    let first = classify(&Disk::new(root, repo), false)?;
    refuse(&first)?;
    report.removed = first.remove.iter().map(|e| e.path.clone()).collect();
    replay(lock, root, repo, Vec::new(), first.remove)?;
    if let (Direction::Back, Some(before)) = (direction, &journal.sparse_paths) {
        // A cone change: what it added went while the new cone still covered
        // it; the old cone comes back to cover what it removed.
        repo.set_metadata(oak_core::MetadataKey::SparsePaths, before)?;
    }
    let second = classify(&Disk::new(root, repo), true)?;
    refuse(&second)?;
    report.written = second.write.iter().map(|e| e.path.clone()).collect();
    report.preserved = second.preserved;
    replay(lock, root, repo, second.write, Vec::new())
}

#[derive(Default)]
struct Classified {
    write: Vec<ManifestEntry>,
    remove: Vec<ManifestEntry>,
    preserved: Vec<Preserved>,
    missing: Vec<(Hash, String)>,
}

impl Classified {
    fn keep(&mut self, path: &str, reason: &'static str) {
        self.preserved.push(Preserved {
            path: path.to_string(),
            reason,
        });
    }
}

/// Write `write` and delete `remove` through the one materializer, so
/// file/directory transitions, empty-directory pruning and the stat cache
/// follow the same rules as the update being replayed.
fn replay(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    mut write: Vec<ManifestEntry>,
    remove: Vec<ManifestEntry>,
) -> Result<()> {
    if write.is_empty() && remove.is_empty() {
        return Ok(());
    }
    write.sort_by(|a, b| a.path.cmp(&b.path));
    let target = Manifest {
        hash: oak_core::Tree::empty_hash(),
        entries: write,
    };
    let old = Manifest {
        hash: oak_core::Tree::empty_hash(),
        entries: remove,
    };
    crate::materialize::apply_manifest_unrecorded(
        lock,
        root,
        repo,
        &target,
        ApplyOpts {
            delete: DeleteScope::TrackedRemoved { old: &old },
            // Every blob was checked above; one that vanished since is an
            // error that keeps the record.
            missing_blobs: MissingBlobs::Error,
            ..ApplyOpts::default()
        },
    )
    .map(|_| ())
}

/// Remove what an interrupted writer left behind: everything in the staging
/// directory under `.oak/`, and — only when the record says this update had
/// to put temp files in the working tree — the files carrying this update's
/// id. No other file is removed, whatever its name: a `.oak.tmp-<uuid>` file
/// of the user's, tracked or not, is theirs.
fn remove_temp_files(oak_dir: &Path, root: &Path, journal: &Journal) -> usize {
    let mut removed = 0;
    if let Ok(entries) = fs::read_dir(oak_dir.join(TEMP_DIR)) {
        for item in entries.flatten() {
            if item.file_type().is_ok_and(|t| !t.is_dir()) && fs::remove_file(item.path()).is_ok() {
                removed += 1;
            }
        }
    }
    if !journal.sibling_temps || journal.id.is_empty() {
        return removed;
    }
    let prefix = sibling_prefix(&journal.id);
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for item in entries.flatten() {
            let name = item.file_name();
            let name = name.to_string_lossy();
            let Ok(file_type) = item.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                // Not Oak's or git's state, and not another repository.
                let path = item.path();
                if name != ".oak"
                    && name != ".git"
                    && !path.join(".oak").exists()
                    && !path.join(".git").exists()
                {
                    pending.push(path);
                }
            } else if name.starts_with(&prefix) && fs::remove_file(item.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Test hook, compiled only into debug builds (what `cargo test` and CI's
/// nextest run): `OAK_TEST_CRASH_AT=<point>` makes the process die at that
/// point as `kill -9` would (no unwinding, no cleanup). A release binary has
/// no such code. Points: `materialize:before` (record written, no file
/// touched), `materialize:<n>` (after the n-th file write),
/// `materialize:after` (every file written, refs not yet published),
/// `write:staged` (a file's bytes written, not yet renamed into place).
pub(crate) struct CrashPoint(Option<String>);

impl CrashPoint {
    pub(crate) fn from_env() -> Self {
        #[cfg(debug_assertions)]
        {
            Self(std::env::var("OAK_TEST_CRASH_AT").ok())
        }
        #[cfg(not(debug_assertions))]
        {
            Self(None)
        }
    }

    pub(crate) fn reached(&self, point: &str) {
        if self.0.as_deref() == Some(point) {
            die();
        }
    }

    pub(crate) fn wrote(&self, count: usize) {
        if let Some(wanted) = &self.0 {
            if wanted
                .strip_prefix("materialize:")
                .and_then(|n| n.parse::<usize>().ok())
                == Some(count)
            {
                die();
            }
        }
    }
}

fn die() -> ! {
    #[cfg(unix)]
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::SqliteRepository;

    const TEMP_NAME: &str = ".oak.tmp-0b0e7c7e-3f0a-4c55-9d5c-2f6c1a0d9e11";

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        oak_dir: PathBuf,
        repo: SqliteRepository,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let oak_dir = root.join(".oak");
        fs::create_dir_all(&oak_dir).unwrap();
        let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
        Fixture {
            _temp: temp,
            root,
            oak_dir,
            repo,
        }
    }

    fn blob(f: &Fixture, path: &str, content: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: f.repo.put_blob(content.as_bytes().to_vec()).unwrap(),
            mode: FileMode::Regular,
        }
    }

    fn write(f: &Fixture, path: &str, content: &str) {
        let full = f.root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    fn read_file(f: &Fixture, path: &str) -> Option<String> {
        fs::read_to_string(f.root.join(path)).ok()
    }

    /// Every file outside `.oak/`, by exact on-disk spelling.
    fn listing(f: &Fixture) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for item in fs::read_dir(dir).unwrap().flatten() {
                let path = item.path();
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if relative == ".oak" {
                    continue;
                }
                if item.file_type().unwrap().is_dir() {
                    walk(root, &path, out);
                } else {
                    out.push(relative);
                }
            }
        }
        let mut out = Vec::new();
        walk(&f.root, &f.root, &mut out);
        out.sort();
        out
    }

    /// Open a record for old -> new and return it as a dead process's.
    fn interrupted(
        f: &Fixture,
        recovery: Recovery,
        target: Vec<ManifestEntry>,
        dropped: &[ManifestEntry],
        replaced: &[ManifestEntry],
        refs_after: Option<Refs>,
    ) {
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        let update = begin(
            &lock,
            &f.repo,
            &f.root,
            recovery,
            Some(Plan {
                target: &Manifest {
                    hash: oak_core::Tree::empty_hash(),
                    entries: target,
                },
                dropped,
                replaced,
                sparse_paths_before: None,
                capture_unknown: true,
                refs_after,
                replaced_is_disk: false,
                on_overlap: OnOverlap::Overwrite,
            }),
        )
        .unwrap();
        std::mem::forget(update);
        edit_record(f, |journal| {
            journal["pid"] = serde_json::json!(u32::MAX - 7)
        });
    }

    // ---- The transition planner -------------------------------------

    fn entry_with_mode(f: &Fixture, path: &str, content: &str, mode: FileMode) -> ManifestEntry {
        ManifestEntry {
            mode,
            ..blob(f, path, content)
        }
    }

    /// Classify base -> target the way `WorktreeDiff` hands it over: `target`
    /// holds the files that differ, `replaced` their old entries, `dropped`
    /// the files only the base has.
    fn plan_and_classify(
        f: &Fixture,
        target: Vec<ManifestEntry>,
        dropped: &[ManifestEntry],
        replaced: &[ManifestEntry],
        on_overlap: OnOverlap,
    ) -> Result<Transition> {
        preflight(
            &f.repo,
            &f.root,
            &Plan {
                target: &Manifest {
                    hash: oak_core::Tree::empty_hash(),
                    entries: target,
                },
                dropped,
                replaced,
                sparse_paths_before: None,
                capture_unknown: true,
                refs_after: None,
                replaced_is_disk: false,
                on_overlap,
            },
        )
    }

    fn overlaps(error: OakError) -> (Vec<(String, String)>, usize) {
        match error {
            OakError::LocalChangesWouldBeOverwritten { paths, total, .. } => (paths, total),
            other => panic!("expected a local-changes refusal, got {other:?}"),
        }
    }

    /// The truth table: per path, (base, disk, target) decides the set.
    #[test]
    fn classify_sorts_every_differing_path_by_what_the_disk_holds() {
        let f = fixture();
        // disk == base, target modifies / deletes / creates.
        let clean_old = blob(&f, "clean.txt", "old\n");
        write(&f, "clean.txt", "old\n");
        let removed_old = blob(&f, "removed.txt", "old\n");
        write(&f, "removed.txt", "old\n");
        // disk == target already.
        let done_old = blob(&f, "done.txt", "old\n");
        write(&f, "done.txt", "new\n");
        // already deleted locally, deleted by target too.
        let both_gone = blob(&f, "both-gone.txt", "old\n");
        // an untracked file whose content is exactly what target creates.
        write(&f, "same-new.txt", "created\n");

        let target = vec![
            blob(&f, "clean.txt", "new\n"),
            blob(&f, "created.txt", "created\n"),
            blob(&f, "done.txt", "new\n"),
            blob(&f, "same-new.txt", "created\n"),
        ];
        let transition = plan_and_classify(
            &f,
            target,
            &[removed_old, both_gone],
            &[clean_old, done_old],
            OnOverlap::Refuse,
        )
        .unwrap();
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        assert_eq!(
            sorted(transition.update),
            ["clean.txt", "created.txt", "removed.txt"]
        );
        assert_eq!(
            sorted(transition.keep),
            ["both-gone.txt", "done.txt", "same-new.txt"]
        );
        assert!(transition.overlap.is_empty());
        // The record lists what will be executed; nothing was written.
        assert_eq!(read_file(&f, "clean.txt").as_deref(), Some("old\n"));
        assert!(!f.oak_dir.join(FILE).exists());
    }

    #[test]
    fn every_kind_of_overlap_refuses_before_anything_is_written_or_recorded() {
        let f = fixture();
        let edited_old = blob(&f, "edited.txt", "old\n");
        write(&f, "edited.txt", "the user's edit\n");
        let deleted_old = blob(&f, "deleted.txt", "old\n");
        let edited_removed_old = blob(&f, "edited-removed.txt", "old\n");
        write(&f, "edited-removed.txt", "the user's edit\n");
        write(&f, "untracked.txt", "the user's untracked file\n");
        let clean_old = blob(&f, "clean.txt", "old\n");
        write(&f, "clean.txt", "old\n");
        let before = listing(&f);

        let target = || {
            vec![
                blob(&f, "clean.txt", "new\n"),
                blob(&f, "deleted.txt", "new\n"),
                blob(&f, "edited.txt", "new\n"),
                blob(&f, "untracked.txt", "created\n"),
            ]
        };
        let dropped = [edited_removed_old];
        let replaced = [edited_old, deleted_old, clean_old];
        let error =
            plan_and_classify(&f, target(), &dropped, &replaced, OnOverlap::Refuse).unwrap_err();
        let (paths, total) = overlaps(error);
        assert_eq!(total, 4);
        assert_eq!(
            paths,
            [
                ("deleted.txt".to_string(), "deleted locally".to_string()),
                (
                    "edited-removed.txt".to_string(),
                    "modified locally, removed by the update".to_string()
                ),
                ("edited.txt".to_string(), "modified locally".to_string()),
                (
                    "untracked.txt".to_string(),
                    "untracked file in the way".to_string()
                ),
            ]
        );

        // `begin` refuses the same way and leaves no record and no change.
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        let result = begin(
            &lock,
            &f.repo,
            &f.root,
            Recovery::Rollback,
            Some(Plan {
                target: &Manifest {
                    hash: oak_core::Tree::empty_hash(),
                    entries: target(),
                },
                dropped: &dropped,
                replaced: &replaced,
                sparse_paths_before: None,
                capture_unknown: true,
                refs_after: None,
                replaced_is_disk: false,
                on_overlap: OnOverlap::Refuse,
            }),
        );
        assert!(matches!(
            result,
            Err(OakError::LocalChangesWouldBeOverwritten { .. })
        ));
        assert!(!f.oak_dir.join(FILE).exists());
        assert_eq!(listing(&f), before);
        assert_eq!(
            read_file(&f, "edited.txt").as_deref(),
            Some("the user's edit\n")
        );
        // Refusing stores nothing: the user's bytes are not copied into the
        // object store.
        assert!(!f
            .repo
            .has_blob(&f.repo.hash_blob_content(b"the user's edit\n"))
            .unwrap());
    }

    #[test]
    fn overwrite_classifies_the_same_overlap_and_proceeds() {
        let f = fixture();
        let edited_old = blob(&f, "edited.txt", "old\n");
        write(&f, "edited.txt", "the user's edit\n");
        let transition = plan_and_classify(
            &f,
            vec![blob(&f, "edited.txt", "new\n")],
            &[],
            &[edited_old],
            OnOverlap::Overwrite,
        )
        .unwrap();
        assert_eq!(
            transition.overlap,
            [Overlap {
                path: "edited.txt".to_string(),
                kind: OverlapKind::Modified
            }]
        );
        // Captured, so a rollback of the forced update restores the edit.
        assert!(f
            .repo
            .has_blob(&f.repo.hash_blob_content(b"the user's edit\n"))
            .unwrap());
    }

    #[test]
    fn a_refusal_names_a_bounded_sample_and_the_total() {
        let f = fixture();
        let mut target = Vec::new();
        for i in 0..(OVERLAP_PATHS_SHOWN + 7) {
            let path = format!("n{i:03}.txt");
            write(&f, &path, "the user's\n");
            target.push(blob(&f, &path, "incoming\n"));
        }
        let error = plan_and_classify(&f, target, &[], &[], OnOverlap::Refuse).unwrap_err();
        let message = error.to_string();
        let (paths, total) = overlaps(error);
        assert_eq!(total, OVERLAP_PATHS_SHOWN + 7);
        assert_eq!(paths.len(), OVERLAP_PATHS_SHOWN);
        assert_eq!(paths[0].0, "n000.txt");
        assert!(message.contains("... and 7 more"), "{message}");
        assert!(message.contains("oak commit"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn mode_and_symlink_differences_count_as_content() {
        let f = fixture();
        // Upstream only flips the executable bit; the disk holds the old
        // mode: clean, updated.
        let script_old = blob(&f, "run.sh", "#!/bin/sh\n");
        write(&f, "run.sh", "#!/bin/sh\n");
        // The user made a file executable and upstream changes its content.
        let tool_old = blob(&f, "tool.sh", "v1\n");
        write(&f, "tool.sh", "v1\n");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(f.root.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The user replaced a tracked file with a symlink; upstream edits it.
        let link_old = blob(&f, "link.txt", "v1\n");
        std::os::unix::fs::symlink("elsewhere", f.root.join("link.txt")).unwrap();
        // Upstream turns a file the user did not touch into a symlink.
        let becomes_link_old = blob(&f, "becomes-link", "v1\n");
        write(&f, "becomes-link", "v1\n");

        let error = plan_and_classify(
            &f,
            vec![
                entry_with_mode(&f, "becomes-link", "target", FileMode::Symlink),
                blob(&f, "link.txt", "v2\n"),
                entry_with_mode(&f, "run.sh", "#!/bin/sh\n", FileMode::Executable),
                blob(&f, "tool.sh", "v2\n"),
            ],
            &[],
            &[script_old, tool_old, link_old, becomes_link_old],
            OnOverlap::Refuse,
        )
        .unwrap_err();
        let (paths, _) = overlaps(error);
        let paths: Vec<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["link.txt", "tool.sh"]);
    }

    /// fb-657: each file/directory obstruction is classified by the planner
    /// and refuses before anything is recorded or written — under both
    /// policies, since nothing writes through it — while the same shapes
    /// built only of tracked, unmodified files are updates.
    #[test]
    fn file_and_directory_obstructions_refuse_before_any_record() {
        let refused = |f: &Fixture,
                       target: Vec<ManifestEntry>,
                       dropped: &[ManifestEntry],
                       want: (&str, OverlapKind)| {
            let before = listing(f);
            for policy in [OnOverlap::Refuse, OnOverlap::Overwrite] {
                let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
                let result = begin(
                    &lock,
                    &f.repo,
                    &f.root,
                    Recovery::Rollback,
                    Some(Plan {
                        target: &Manifest {
                            hash: oak_core::Tree::empty_hash(),
                            entries: target.clone(),
                        },
                        dropped,
                        replaced: &[],
                        sparse_paths_before: None,
                        capture_unknown: true,
                        refs_after: None,
                        replaced_is_disk: false,
                        on_overlap: policy,
                    }),
                );
                let error = match result {
                    Err(error) => error,
                    Ok(_) => panic!("{want:?} under {policy:?} must refuse"),
                };
                let (paths, _) = overlaps(error);
                assert_eq!(
                    paths,
                    [(want.0.to_string(), want.1.describe().to_string())],
                    "{policy:?}"
                );
                assert!(!f.oak_dir.join(FILE).exists(), "no record");
                assert_eq!(listing(f), before, "nothing written");
            }
        };

        // An untracked (or ignored) directory where the update adds a file.
        let f = fixture();
        write(&f, "nd/mine", "the user's\n");
        refused(
            &f,
            vec![blob(&f, "nd", "incoming\n")],
            &[],
            ("nd", OverlapKind::DirectoryInTheWay),
        );
        // An untracked file where the update adds a directory.
        let f = fixture();
        write(&f, "nf", "the user's\n");
        refused(
            &f,
            vec![blob(&f, "nf/x", "incoming\n")],
            &[],
            ("nf", OverlapKind::InTheWayOfDirectory),
        );
        // An untracked file inside a directory the update turns into a file.
        let f = fixture();
        let inner = blob(&f, "df/a", "a\n");
        write(&f, "df/a", "a\n");
        write(&f, "df/mine-untracked", "the user's\n");
        refused(
            &f,
            vec![blob(&f, "df", "now a file\n")],
            std::slice::from_ref(&inner),
            ("df", OverlapKind::DirectoryInTheWay),
        );
        // A tracked file edited inside that directory.
        let f = fixture();
        let inner = blob(&f, "df/a", "a\n");
        write(&f, "df/a", "the user's edit\n");
        refused(
            &f,
            vec![blob(&f, "df", "now a file\n")],
            std::slice::from_ref(&inner),
            ("df/a", OverlapKind::ModifiedInReplacedDirectory),
        );
        // An untracked symlink where the update adds a directory.
        #[cfg(unix)]
        {
            let f = fixture();
            std::os::unix::fs::symlink("elsewhere", f.root.join("sd")).unwrap();
            refused(
                &f,
                vec![blob(&f, "sd/x", "incoming\n")],
                &[],
                ("sd", OverlapKind::InTheWayOfDirectory),
            );
        }

        // The same shapes made only of the snapshot's own unmodified files,
        // or empty, are ordinary updates.
        let f = fixture();
        let inner = blob(&f, "d/a", "inner\n");
        write(&f, "d/a", "inner\n");
        fs::create_dir_all(f.root.join("empty")).unwrap();
        let file_old = blob(&f, "x", "file\n");
        write(&f, "x", "file\n");
        let transition = plan_and_classify(
            &f,
            vec![
                blob(&f, "d", "now a file\n"),
                blob(&f, "empty", "now a file\n"),
                blob(&f, "x/inner", "inner\n"),
            ],
            &[inner, file_old.clone()],
            &[],
            OnOverlap::Refuse,
        )
        .unwrap();
        assert!(transition.overlap.is_empty(), "{:?}", transition.overlap);
        // A tracked file edited where a directory goes is the user's edit.
        write(&f, "x", "edited\n");
        let error = plan_and_classify(
            &f,
            vec![blob(&f, "x/inner", "inner\n")],
            &[file_old],
            &[],
            OnOverlap::Refuse,
        )
        .unwrap_err();
        assert_eq!(overlaps(error).0[0].0, "x");
    }

    /// An ignored file at a path the update creates is named as ignored: a
    /// commit would not pick it up, so the advice is to move it.
    #[test]
    fn an_ignored_file_in_the_way_is_named_as_ignored() {
        let f = fixture();
        write(&f, ".oakignore", "*.log\n");
        write(&f, "x.log", "the user's log\n");
        let error = plan_and_classify(
            &f,
            vec![blob(&f, "x.log", "tracked now\n")],
            &[],
            &[],
            OnOverlap::Refuse,
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                OakError::LocalChangesWouldBeOverwritten {
                    in_the_way: true,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            overlaps(error).0,
            [("x.log".to_string(), "ignored file in the way".to_string())]
        );
    }

    /// fb-656: on a case-insensitive filesystem an untracked `case.txt` IS
    /// the `Case.txt` the update creates; it is in the way, listed under its
    /// own spelling. (On a case-sensitive filesystem they are two files and
    /// nothing is in the way.)
    #[test]
    fn an_untracked_case_twin_of_a_created_path_is_in_the_way() {
        let f = fixture();
        write(&f, "case.txt", "the user's\n");
        let folds = f.root.join("CASE.TXT").exists();
        let result = plan_and_classify(
            &f,
            vec![blob(&f, "Case.txt", "incoming\n")],
            &[],
            &[],
            OnOverlap::Refuse,
        );
        if folds {
            let error = result.unwrap_err();
            assert!(
                matches!(
                    &error,
                    OakError::LocalChangesWouldBeOverwritten {
                        in_the_way: true,
                        ..
                    }
                ),
                "{error:?}"
            );
            assert_eq!(
                overlaps(error).0,
                [(
                    "case.txt".to_string(),
                    OverlapKind::CaseTwin.describe().to_string()
                )]
            );
        } else {
            assert!(result.unwrap().overlap.is_empty());
        }
        assert_eq!(read_file(&f, "case.txt").as_deref(), Some("the user's\n"));
        assert!(!f.oak_dir.join(FILE).exists());
    }

    /// APFS and HFS+ also fold Unicode normalization: an untracked file named
    /// in NFD (`cafe` + combining acute) is the same entry as the NFC
    /// `café.txt` the update creates, so writing it would replace the user's
    /// bytes. Also under a twin directory, and combined with a case change.
    #[test]
    fn an_untracked_normalization_twin_of_a_created_path_is_in_the_way() {
        for (on_disk, created) in [
            ("cafe\u{301}.txt", "caf\u{e9}.txt"),
            ("nai\u{308}ve/x.txt", "na\u{ef}ve/x.txt"),
            ("cafe\u{301}.txt", "Caf\u{e9}.txt"),
        ] {
            let f = fixture();
            write(&f, on_disk, "the user's\n");
            let folds = f.root.join(created).exists();
            let result = plan_and_classify(
                &f,
                vec![blob(&f, created, "incoming\n")],
                &[],
                &[],
                OnOverlap::Refuse,
            );
            if folds {
                let error = result.unwrap_err();
                assert!(
                    matches!(
                        &error,
                        OakError::LocalChangesWouldBeOverwritten {
                            in_the_way: true,
                            ..
                        }
                    ),
                    "{on_disk} vs {created}: {error:?}"
                );
                assert_eq!(overlaps(error).0.len(), 1, "{on_disk} vs {created}");
            } else {
                assert!(result.unwrap().overlap.is_empty(), "{on_disk} vs {created}");
            }
            assert_eq!(
                read_file(&f, on_disk).as_deref(),
                Some("the user's\n"),
                "{on_disk} vs {created}"
            );
            assert!(!f.oak_dir.join(FILE).exists());
        }
    }

    #[test]
    fn a_case_only_rename_of_an_untouched_file_is_not_an_overlap() {
        let f = fixture();
        let old = blob(&f, "readme.md", "text\n");
        write(&f, "readme.md", "text\n");
        let transition = plan_and_classify(
            &f,
            vec![blob(&f, "README.md", "text\n")],
            std::slice::from_ref(&old),
            &[],
            OnOverlap::Refuse,
        )
        .unwrap();
        assert!(transition.overlap.is_empty(), "{:?}", transition.overlap);
        // Edited under the old spelling: the edit is in the way.
        write(&f, "readme.md", "edited\n");
        let error = plan_and_classify(
            &f,
            vec![blob(&f, "README.md", "text\n")],
            &[old],
            &[],
            OnOverlap::Refuse,
        )
        .unwrap_err();
        assert!(overlaps(error).1 >= 1);
    }

    #[test]
    fn paths_outside_the_sparse_cone_and_withheld_files_are_not_local_changes() {
        let f = fixture();
        f.repo
            .set_metadata(
                oak_core::MetadataKey::SparsePaths,
                &oak_core::SparseCone::new(vec!["in".to_string()])
                    .unwrap()
                    .to_metadata(),
            )
            .unwrap();
        // Outside the cone: never on disk, never classified.
        let outside_old = blob(&f, "out/a.txt", "old\n");
        // Inside, withheld by the server: tracked, never written.
        let withheld_old = blob(&f, "in/secret.txt", "secret\n");
        crate::commands::restricted::record_restricted_blobs(
            &f.repo,
            &[withheld_old.blob_hash.to_string()],
        )
        .unwrap();
        // Inside, tracked, content never fetched: never written either.
        let unhydrated_old = ManifestEntry {
            path: "in/lazy.txt".to_string(),
            blob_hash: f.repo.hash_blob_content(b"never fetched\n"),
            mode: FileMode::Regular,
        };
        // Inside and really deleted by the user.
        let deleted_old = blob(&f, "in/deleted.txt", "old\n");
        fs::create_dir_all(f.root.join("in")).unwrap();
        let error = plan_and_classify(
            &f,
            vec![
                blob(&f, "in/deleted.txt", "new\n"),
                blob(&f, "in/lazy.txt", "new lazy\n"),
                blob(&f, "in/secret.txt", "new secret\n"),
                blob(&f, "out/a.txt", "new\n"),
            ],
            &[],
            &[outside_old, withheld_old, unhydrated_old, deleted_old],
            OnOverlap::Refuse,
        )
        .unwrap_err();
        let (paths, total) = overlaps(error);
        assert_eq!(total, 1, "{paths:?}");
        assert_eq!(paths[0].0, "in/deleted.txt");
    }

    /// Every call that writes tracked content into the working tree goes
    /// through the planner, or is listed here with the reason it need not.
    /// Adding a call site changes a count and fails this test: route the new
    /// site through `WorktreeDiff` / `WorkingDirMove` (planned, refuses on
    /// overlap), or add it here and say why overwriting is right.
    #[test]
    fn no_tree_writer_bypasses_the_planner() {
        // (file, token, expected occurrences outside tests and comments, why)
        const ALLOWED: &[(&str, &str, usize, &str)] = &[
            // The planner and the materializer themselves.
            ("worktree_update.rs", "apply_manifest_unrecorded(", 1, "recovery replays a recorded plan"),
            ("worktree_update.rs", "OnOverlap::Overwrite", 1, "the capture switch inside classify"),
            ("materialize.rs", "apply_manifest(", 1, "definition"),
            ("materialize.rs", "apply_manifest_unrecorded(", 2, "definition and its one caller"),
            ("materialize.rs", "materialize_path(", 1, "definition"),
            ("materialize.rs", "worktree_update::begin(", 1, "apply_manifest's own Clear record"),
            // Planned transitions: every one of these is classified first.
            ("commands/merge_tree.rs", "apply_manifest(", 1, "WorktreeDiff::apply_with, after begin() classified the plan"),
            ("commands/merge_tree.rs", "worktree_update::begin(", 2, "WorktreeDiff::apply_with and begin_reset_after_refs: both pass a Plan"),
            ("commands/merge_tree.rs", "OnOverlap::Overwrite", 2, "pull --force (rewrite_everything); reset with no known old snapshot, whose callers proved the tree clean"),
            ("commands/merge_tree.rs", "reset_to_manifest(", 1, "ResetAfterRefs::write when the old snapshot is unknown (callers proved the tree clean)"),
            ("commands/sparse.rs", "worktree_update::begin(", 1, "cone change: passes a Plan, refuses on overlap"),
            ("commands/sparse.rs", "apply_manifest(", 1, "writes the entering files classified by the Plan above"),
            ("commands/switch.rs", "worktree_update::begin(", 1, "record_whole_tree_move: --clean, or a tree a clean guard passed"),
            ("commands/switch.rs", "OnOverlap::Overwrite", 1, "record_whole_tree_move, as above"),
            ("commands/switch.rs", "apply_manifest(", 1, "update_working_dir: the whole-tree rewrite behind --clean / init / clone"),
            ("commands/switch.rs", "update_working_dir(", 2, "definition; MovePlan::Full (--clean or proved clean)"),
            ("commands/switch/journal.rs", "apply_manifest(", 1, "switch recovery replays the journal's recorded paths"),
            ("commands/pull.rs", "update_working_dir(", 3, "pull's own diff-based update (definition + 2 callers): WorktreeDiff, classified"),
            // Writing toward the snapshot the refs already name, at the
            // user's explicit request to discard.
            ("commands/reset.rs", "apply_manifest(", 2, "oak reset: the user asks to discard"),
            ("commands/reset.rs", "reset_to_manifest(", 1, "definition"),
            ("commands/restore.rs", "apply_manifest(", 1, "oak restore: the user asks to discard"),
            ("commands/merge.rs", "apply_manifest(", 1, "merge/pull --abort: undoes exactly the paths the conflict write-out changed"),
            ("commands/sync.rs", "reset_to_manifest(", 1, "pull --continue of a sync paused by an older binary: commits and resets to the user's resolved whole-tree snapshot"),
            // Only where nothing is on disk.
            ("commands/hydrate.rs", "apply_manifest(", 1, "hydration writes files that are absent"),
            ("commands/hydrate.rs", "write_unwritten_files(", 1, "definition"),
            ("commands/pull.rs", "write_unwritten_files(", 2, "newly granted files, only where nothing is on disk"),
            // A tree that is new or not a checkout of this repository.
            ("commands/init.rs", "update_working_dir(", 1, "fresh repository"),
            ("commands/git_clone.rs", "update_working_dir(", 1, "fresh clone"),
            ("commands/root.rs", "materialize_path(", 1, "oak root pull: the separate personal-root timeline under me/ (not journaled; F4 owns it)"),
            ("commands/repo.rs", "materialize_path(", 1, "oak repo move: writes into the destination repository's import directory"),
        ];
        const TOKENS: &[&str] = &[
            "apply_manifest(",
            "apply_manifest_unrecorded(",
            "materialize_path(",
            "update_working_dir(",
            "reset_to_manifest(",
            "write_unwritten_files(",
            "worktree_update::begin(",
            "OnOverlap::Overwrite",
        ];
        fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
            for item in fs::read_dir(dir).unwrap().flatten() {
                let path = item.path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&src, &mut files);
        let mut found: Vec<(String, &str, usize)> = Vec::new();
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            // Unit tests build fixtures by writing trees directly.
            let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            let code: String = code
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let name = file
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for token in TOKENS {
                let count = code.matches(token).count();
                if count > 0 {
                    found.push((name.clone(), token, count));
                }
            }
        }
        let mut problems = Vec::new();
        for (file, token, count) in &found {
            match ALLOWED.iter().find(|(f, t, _, _)| f == file && t == token) {
                Some((_, _, expected, _)) if expected == count => {}
                Some((_, _, expected, why)) => problems.push(format!(
                    "{file}: `{token}` appears {count} time(s), expected {expected} ({why})"
                )),
                None => problems.push(format!(
                    "{file}: `{token}` appears {count} time(s) and is not listed"
                )),
            }
        }
        for (file, token, _, _) in ALLOWED {
            if !found.iter().any(|(f, t, _)| f == file && t == token) {
                problems.push(format!("{file}: `{token}` is listed but no longer present"));
            }
        }
        assert!(
            problems.is_empty(),
            "working-tree writers changed; see the comment on this test:\n  {}",
            problems.join("\n  ")
        );
    }

    fn edit_record(f: &Fixture, edit: impl FnOnce(&mut serde_json::Value)) {
        let file = f.oak_dir.join(FILE);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        edit(&mut journal);
        fs::write(&file, serde_json::to_vec(&journal).unwrap()).unwrap();
    }

    fn run_recovery(f: &Fixture, unattended: bool) -> Result<Option<Recovered>> {
        let lock = WorkdirLock::acquire_for_update_recovery(&f.oak_dir)?;
        recover(&lock, &f.oak_dir, &f.root, &f.repo, unattended)
    }

    /// old -> new, killed after writing `a` (modified) and `dir/added` but
    /// before `b` (modified) and the removal of `gone`.
    fn torn_rollback(f: &Fixture) {
        let old_a = blob(f, "a.txt", "old a\n");
        let old_b = blob(f, "b.txt", "old b\n");
        let gone = blob(f, "gone.txt", "gone\n");
        for (path, content) in [
            ("a.txt", "old a\n"),
            ("b.txt", "old b\n"),
            ("gone.txt", "gone\n"),
            ("mine.txt", "untracked user file\n"),
        ] {
            write(f, path, content);
        }
        let target = vec![
            blob(f, "a.txt", "new a\n"),
            blob(f, "b.txt", "new b\n"),
            blob(f, "dir/added.txt", "added\n"),
        ];
        interrupted(
            f,
            Recovery::Rollback,
            target,
            &[gone],
            &[old_a, old_b],
            None,
        );
        write(f, "a.txt", "new a\n");
        write(f, "dir/added.txt", "added\n");
        // A file it was in the middle of writing.
        fs::create_dir_all(f.oak_dir.join(TEMP_DIR)).unwrap();
        fs::write(f.oak_dir.join(TEMP_DIR).join(TEMP_NAME), "half").unwrap();
    }

    #[test]
    fn interrupted_update_is_reported_and_blocks_the_lock() {
        let f = fixture();
        torn_rollback(&f);
        let reported = pending(&f.oak_dir).expect("record is reported");
        assert_eq!(reported.next_commands[0], "oak pull --abort");
        assert!(reported.message.contains("oak pull --abort"));
        assert!(
            reported.message.contains("is put back as it was"),
            "{}",
            reported.message
        );
        match WorkdirLock::acquire(&f.oak_dir) {
            Err(OakError::InterruptedWorktreeUpdate { next_commands, .. }) => {
                assert_eq!(next_commands, reported.next_commands);
            }
            other => panic!("expected the lock to be refused, got {:?}", other.err()),
        }
        assert!(
            !f.oak_dir.join("wdlock").exists(),
            "the refusal released it"
        );
        // A command that leaves the working tree alone is not refused.
        drop(WorkdirLock::acquire_leaving_worktree(&f.oak_dir).unwrap());
    }

    #[test]
    fn rollback_restores_what_the_update_wrote_and_reports_what_it_left() {
        let f = fixture();
        torn_rollback(&f);
        // Edited after the interruption, at a path the update had not reached.
        write(&f, "b.txt", "edited after the crash\n");

        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(recovered.action, "rolled_back");
        assert_eq!(recovered.written, vec!["a.txt"]);
        assert_eq!(recovered.removed, vec!["dir/added.txt"]);
        assert_eq!(
            recovered.preserved,
            vec![Preserved {
                path: "b.txt".to_string(),
                reason: NOT_THE_UPDATES
            }]
        );
        assert!(recovered.message().contains("b.txt"));
        assert_eq!(recovered.temp_files_removed, 1);
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("old a\n"));
        assert_eq!(
            read_file(&f, "b.txt").as_deref(),
            Some("edited after the crash\n")
        );
        assert_eq!(
            listing(&f),
            vec!["a.txt", "b.txt", "gone.txt", "mine.txt"],
            "the added file and its directory are gone, nothing else moved"
        );
        assert!(!f.oak_dir.join(FILE).exists());
        assert!(run_recovery(&f, false).unwrap().is_none(), "idempotent");
    }

    #[test]
    fn rollback_restores_a_dropped_file_but_not_one_the_user_deleted_first() {
        let f = fixture();
        let dropped = blob(&f, "dropped.txt", "dropped\n");
        let user_deleted = blob(&f, "user-deleted.txt", "x\n");
        write(&f, "dropped.txt", "dropped\n");
        interrupted(
            &f,
            Recovery::Rollback,
            Vec::new(),
            &[dropped, user_deleted],
            &[],
            None,
        );
        fs::remove_file(f.root.join("dropped.txt")).unwrap();
        run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(listing(&f), vec!["dropped.txt"]);
    }

    /// Reviewer item 2. The update replaced a tracked file with a directory;
    /// the file can only come back once the update's files in it are gone.
    #[test]
    fn rollback_restores_a_file_the_update_replaced_with_a_directory() {
        for killed_during_recovery in [false, true] {
            let f = fixture();
            let old = blob(&f, "x", "x was a file\n");
            write(&f, "x", "x was a file\n");
            let target = vec![blob(&f, "x/a.txt", "a\n"), blob(&f, "x/b.txt", "b\n")];
            interrupted(&f, Recovery::Rollback, target, &[old], &[], None);
            fs::remove_file(f.root.join("x")).unwrap();
            write(&f, "x/a.txt", "a\n");
            if killed_during_recovery {
                // A first recovery removed the update's files and died.
                fs::remove_file(f.root.join("x/a.txt")).unwrap();
                fs::remove_dir(f.root.join("x")).unwrap();
            }
            let recovered = run_recovery(&f, false).unwrap().unwrap();
            assert_eq!(recovered.written, vec!["x"]);
            assert_eq!(recovered.preserved, Vec::new());
            assert_eq!(listing(&f), vec!["x"]);
            assert_eq!(read_file(&f, "x").as_deref(), Some("x was a file\n"));
        }
    }

    #[test]
    fn rollback_restores_a_directory_the_update_replaced_with_a_file() {
        let f = fixture();
        let old = blob(&f, "d/inner.txt", "inner\n");
        write(&f, "d/inner.txt", "inner\n");
        interrupted(
            &f,
            Recovery::Rollback,
            vec![blob(&f, "d", "now a file\n")],
            &[old],
            &[],
            None,
        );
        fs::remove_dir_all(f.root.join("d")).unwrap();
        write(&f, "d", "now a file\n");
        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(recovered.preserved, Vec::new());
        assert_eq!(listing(&f), vec!["d/inner.txt"]);
    }

    #[test]
    fn a_directory_holding_other_content_is_left_and_reported() {
        let f = fixture();
        let old = blob(&f, "x", "x was a file\n");
        write(&f, "x", "x was a file\n");
        interrupted(
            &f,
            Recovery::Rollback,
            vec![blob(&f, "x/a.txt", "a\n")],
            &[old],
            &[],
            None,
        );
        fs::remove_file(f.root.join("x")).unwrap();
        write(&f, "x/a.txt", "a\n");
        write(&f, "x/mine.txt", "made after the crash\n");
        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(
            recovered.preserved,
            vec![Preserved {
                path: "x".to_string(),
                reason: IN_THE_WAY
            }]
        );
        assert_eq!(listing(&f), vec!["x/mine.txt"]);
    }

    /// Reviewer item 3. On a case-insensitive filesystem the new spelling
    /// reaches the old file, so "exists" must mean "exists under this name".
    #[test]
    fn rollback_undoes_a_case_only_rename() {
        let f = fixture();
        write(&f, "Probe", "");
        if !f.root.join("probe").exists() {
            return; // case-sensitive filesystem: the two names never alias
        }
        fs::remove_file(f.root.join("Probe")).unwrap();
        for written in [false, true] {
            let old = blob(&f, "Case.txt", "case old\n");
            let old_inner = blob(&f, "Dir/inner.txt", "inner\n");
            write(&f, "Case.txt", "case old\n");
            write(&f, "Dir/inner.txt", "inner\n");
            let target = vec![
                blob(&f, "case.txt", "case NEW\n"),
                blob(&f, "dir/inner.txt", "inner\n"),
            ];
            interrupted(&f, Recovery::Rollback, target, &[old, old_inner], &[], None);
            if written {
                fs::rename(f.root.join("Case.txt"), f.root.join("case.txt")).unwrap();
                write(&f, "case.txt", "case NEW\n");
                fs::rename(f.root.join("Dir"), f.root.join("dir")).unwrap();
            }
            let recovered = run_recovery(&f, false).unwrap().unwrap();
            assert_eq!(recovered.preserved, Vec::new(), "written={written}");
            assert_eq!(
                listing(&f),
                vec!["Case.txt", "Dir/inner.txt"],
                "written={written}"
            );
            assert_eq!(read_file(&f, "Case.txt").as_deref(), Some("case old\n"));
        }
    }

    /// An untracked file at an incoming path is overwritten by the update
    /// itself; the record keeps its content so a rollback brings it back.
    #[test]
    fn rollback_restores_an_untracked_file_the_update_overwrote() {
        let f = fixture();
        write(&f, "new.txt", "the user's untracked file\n");
        interrupted(
            &f,
            Recovery::Rollback,
            vec![blob(&f, "new.txt", "incoming\n")],
            &[],
            &[],
            None,
        );
        write(&f, "new.txt", "incoming\n");
        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(recovered.written, vec!["new.txt"]);
        assert_eq!(
            read_file(&f, "new.txt").as_deref(),
            Some("the user's untracked file\n")
        );
    }

    /// Reviewer item 6: an uncommitted edit the update overwrites comes
    /// back when it could be told from the committed file, and a file that
    /// already held the incoming content is not "restored" away from it.
    #[test]
    fn rollback_restores_an_uncommitted_edit_the_update_overwrote() {
        let f = fixture();
        let old_a = blob(&f, "a.txt", "committed a\n");
        let old_b = blob(&f, "b.txt", "committed b\n");
        let old_c = blob(&f, "c.txt", "committed c\n");
        write(&f, "a.txt", "the user's longer uncommitted edit\n");
        write(&f, "b.txt", "incoming b, typed in by the user\n");
        // Review N2: an edit that keeps the committed file's exact size.
        write(&f, "c.txt", "COMMITTED C\n");
        let target = vec![
            blob(&f, "a.txt", "incoming a\n"),
            blob(&f, "b.txt", "incoming b, typed in by the user\n"),
            blob(&f, "c.txt", "incoming c\n"),
        ];
        interrupted(
            &f,
            Recovery::Rollback,
            target,
            &[],
            &[old_a, old_b, old_c],
            None,
        );
        write(&f, "a.txt", "incoming a\n");
        write(&f, "c.txt", "incoming c\n");
        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert_eq!(recovered.written, vec!["a.txt", "c.txt"]);
        assert_eq!(read_file(&f, "c.txt").as_deref(), Some("COMMITTED C\n"));
        assert_eq!(
            read_file(&f, "a.txt").as_deref(),
            Some("the user's longer uncommitted edit\n")
        );
        assert_eq!(
            read_file(&f, "b.txt").as_deref(),
            Some("incoming b, typed in by the user\n")
        );
    }

    /// Reviewer item 4.
    #[test]
    fn an_unreadable_record_blocks_and_is_never_cleared() {
        for damage in [
            "",
            "{\"version\":1,\"operation\":\"pu",
            "\u{0}\u{1} not json",
        ] {
            let f = fixture();
            torn_rollback(&f);
            let before = listing(&f);
            fs::write(f.oak_dir.join(FILE), damage).unwrap();

            let reported = pending(&f.oak_dir).expect("still reported");
            assert_eq!(reported.next_commands, vec!["oak diff --json"]);
            assert!(
                reported.message.contains("cannot be read"),
                "{}",
                reported.message
            );
            assert!(matches!(
                WorkdirLock::acquire(&f.oak_dir),
                Err(OakError::InterruptedWorktreeUpdate { .. })
            ));
            for unattended in [false, true] {
                assert!(matches!(
                    run_recovery(&f, unattended),
                    Err(OakError::InterruptedWorktreeUpdate { .. })
                ));
            }
            assert_eq!(fs::read_to_string(f.oak_dir.join(FILE)).unwrap(), damage);
            assert_eq!(listing(&f), before, "no file was touched");
        }
    }

    #[test]
    fn a_record_from_a_newer_oak_is_refused_not_discarded() {
        let f = fixture();
        torn_rollback(&f);
        edit_record(&f, |journal| {
            journal["version"] = serde_json::json!(VERSION + 1);
            journal["recovery"] = serde_json::json!("something_new");
        });
        let reported = pending(&f.oak_dir).unwrap();
        assert_eq!(reported.next_commands, vec!["oak upgrade"]);
        assert!(matches!(
            run_recovery(&f, false),
            Err(OakError::InterruptedWorktreeUpdate { .. })
        ));
        assert!(f.oak_dir.join(FILE).exists());
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("new a\n"));
    }

    /// Reviewer item 5.
    #[test]
    fn a_missing_restore_blob_fails_and_keeps_the_record() {
        let f = fixture();
        torn_rollback(&f);
        let absent = f.repo.hash_blob_content(b"never stored");
        edit_record(&f, |journal| {
            journal["paths"][0]["before"]["file"]["hash"] = serde_json::json!(absent.as_str());
        });
        let before = listing(&f);
        match run_recovery(&f, false) {
            Err(OakError::InterruptedWorktreeUpdate { message, .. }) => {
                assert!(message.contains("a.txt"), "{message}");
                assert!(message.contains("not in local storage"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(f.oak_dir.join(FILE).exists());
        assert_eq!(listing(&f), before);
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("new a\n"));
        assert_eq!(read_file(&f, "dir/added.txt").as_deref(), Some("added\n"));
    }

    #[test]
    fn moved_refs_supersede_the_record() {
        let f = fixture();
        torn_rollback(&f);
        // Another binary carried on and moved the branch.
        f.repo.set_current_branch("elsewhere").unwrap();
        let recovered = run_recovery(&f, false).unwrap().unwrap();
        assert!(recovered.superseded);
        assert_eq!(recovered.action, "cleared");
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("new a\n"));
        assert_eq!(read_file(&f, "dir/added.txt").as_deref(), Some("added\n"));
    }

    /// Reviewer item 1: an update whose refs move first is finished, never
    /// just cleared — unless it died before they moved.
    #[test]
    fn a_refs_first_update_is_completed_once_its_refs_moved() {
        for refs_moved in [true, false] {
            let f = fixture();
            let old_a = blob(&f, "a.txt", "old a\n");
            let old_b = blob(&f, "b.txt", "old b\n");
            let gone = blob(&f, "gone.txt", "gone\n");
            for (path, content) in [
                ("a.txt", "old a\n"),
                ("b.txt", "old b\n"),
                ("gone.txt", "gone\n"),
            ] {
                write(&f, path, content);
            }
            let target = vec![
                blob(&f, "a.txt", "new a\n"),
                blob(&f, "b.txt", "new b\n"),
                blob(&f, "new.txt", "new\n"),
            ];
            interrupted(
                &f,
                Recovery::Complete,
                target,
                &[gone],
                &[old_a, old_b],
                Some(Refs {
                    branch: Some("fresh".to_string()),
                    head: None,
                    branch_head: None,
                }),
            );
            if refs_moved {
                f.repo.set_current_branch("fresh").unwrap();
                write(&f, "a.txt", "new a\n");
                // Edited after the kill: neither side, so it stays.
                write(&f, "b.txt", "edited after the crash\n");
            }
            let recovered = run_recovery(&f, false).unwrap().unwrap();
            if refs_moved {
                assert_eq!(recovered.action, "completed");
                assert_eq!(recovered.written, vec!["new.txt"]);
                assert_eq!(recovered.removed, vec!["gone.txt"]);
                assert_eq!(recovered.preserved[0].path, "b.txt");
                assert_eq!(listing(&f), vec!["a.txt", "b.txt", "new.txt"]);
                assert_eq!(read_file(&f, "a.txt").as_deref(), Some("new a\n"));
                assert_eq!(
                    read_file(&f, "b.txt").as_deref(),
                    Some("edited after the crash\n")
                );
            } else {
                assert_eq!(recovered.action, "rolled_back");
                assert_eq!(listing(&f), vec!["a.txt", "b.txt", "gone.txt"]);
                assert_eq!(read_file(&f, "a.txt").as_deref(), Some("old a\n"));
            }
        }
    }

    #[test]
    fn unattended_recovery_only_acts_when_nothing_is_left_to_decide() {
        // A record that only clears stays, and stays blocking.
        let f = fixture();
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        std::mem::forget(begin(&lock, &f.repo, &f.root, Recovery::Clear, None).unwrap());
        drop(lock);
        edit_record(&f, |journal| {
            journal["pid"] = serde_json::json!(u32::MAX - 7)
        });
        assert!(matches!(
            run_recovery(&f, true),
            Err(OakError::InterruptedWorktreeUpdate { .. })
        ));
        assert!(f.oak_dir.join(FILE).exists());
        assert_eq!(run_recovery(&f, false).unwrap().unwrap().action, "cleared");

        // A rollback that would leave a path for the user stays too, with
        // the tree untouched.
        let f = fixture();
        torn_rollback(&f);
        write(&f, "b.txt", "edited after the crash\n");
        assert!(matches!(
            run_recovery(&f, true),
            Err(OakError::InterruptedWorktreeUpdate { .. })
        ));
        assert!(f.oak_dir.join(FILE).exists());
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("new a\n"));

        // A clean rollback is taken.
        let f = fixture();
        torn_rollback(&f);
        assert_eq!(
            run_recovery(&f, true).unwrap().unwrap().action,
            "rolled_back"
        );
        assert_eq!(read_file(&f, "a.txt").as_deref(), Some("old a\n"));
    }

    /// Review N1 (and 9). Only what this update itself left is removed: the
    /// staging directory's contents, and — when the record says the update
    /// had to write temp files into the tree — files carrying its id. A
    /// file that merely has the `.oak.tmp-<uuid>` shape is the user's.
    #[test]
    fn only_this_updates_temp_files_are_removed() {
        for fallback_used in [false, true] {
            let f = fixture();
            torn_rollback(&f);
            let record: Journal =
                serde_json::from_slice(&fs::read(f.oak_dir.join(FILE)).unwrap()).unwrap();
            let own = format!("{}0123abcd", sibling_prefix(&record.id));
            if fallback_used {
                edit_record(&f, |journal| {
                    journal["sibling_temps"] = serde_json::json!(true)
                });
                write(&f, &format!("dir/{own}"), "half");
                write(&f, &format!("nested/{own}"), "in a nested repository");
            }
            // The user's, in directories the update wrote to and elsewhere.
            write(&f, TEMP_NAME, "the user's, at the root");
            write(
                &f,
                &format!("dir/{TEMP_NAME}"),
                "the user's, next to an added file",
            );
            write(&f, &format!("elsewhere/{TEMP_NAME}"), "the user's");
            write(
                &f,
                &format!("nested/.oak/{TEMP_NAME}"),
                "a nested repository's",
            );

            let recovered = run_recovery(&f, false).unwrap().unwrap();
            assert_eq!(recovered.temp_files_removed, 1 + usize::from(fallback_used));
            let mut expected = vec![
                TEMP_NAME.to_string(),
                "a.txt".to_string(),
                "b.txt".to_string(),
                format!("dir/{TEMP_NAME}"),
                format!("elsewhere/{TEMP_NAME}"),
                "gone.txt".to_string(),
                "mine.txt".to_string(),
                format!("nested/.oak/{TEMP_NAME}"),
            ];
            if fallback_used {
                expected.push(format!("nested/{own}"));
            }
            expected.sort();
            assert_eq!(listing(&f), expected, "fallback_used={fallback_used}");
            assert_eq!(fs::read_dir(f.oak_dir.join(TEMP_DIR)).unwrap().count(), 0);
        }
    }

    /// The sibling fallback marks the record before it creates its first
    /// file, and names the file after the record.
    #[test]
    fn a_sibling_temp_is_named_after_its_record_and_announced_first() {
        let f = fixture();
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        let update = begin(&lock, &f.repo, &f.root, Recovery::Clear, None).unwrap();
        let staging = Staging::for_update(&lock);
        let record = |f: &Fixture| -> Journal {
            serde_json::from_slice(&fs::read(f.oak_dir.join(FILE)).unwrap()).unwrap()
        };
        assert!(!record(&f).sibling_temps);
        let sibling = staging.sibling(&f.root.join("sub/file.txt")).unwrap();
        let journal = record(&f);
        assert!(journal.sibling_temps);
        assert_eq!(sibling.parent().unwrap(), f.root.join("sub"));
        let name = sibling.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&sibling_prefix(&journal.id)), "{name}");
        update.finish();
    }

    #[test]
    fn a_nested_update_is_covered_by_the_enclosing_record() {
        let f = fixture();
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        let outer = begin(&lock, &f.repo, &f.root, Recovery::Rollback, None).unwrap();
        assert!(covered(&lock));
        let inner = begin(&lock, &f.repo, &f.root, Recovery::Clear, None).unwrap();
        inner.finish();
        assert!(
            f.oak_dir.join(FILE).exists(),
            "inner finish keeps the outer"
        );
        outer.finish();
        assert!(!f.oak_dir.join(FILE).exists());
    }

    #[test]
    fn a_live_update_is_not_reported_as_interrupted() {
        let f = fixture();
        let lock = WorkdirLock::acquire(&f.oak_dir).unwrap();
        let update = begin(&lock, &f.repo, &f.root, Recovery::Clear, None).unwrap();
        assert!(pending(&f.oak_dir).is_none());
        update.finish();
    }

    #[test]
    fn the_crash_hook_exists_only_in_debug_builds() {
        std::env::set_var("OAK_TEST_CRASH_AT", "never-reached");
        assert_eq!(CrashPoint::from_env().0.is_some(), cfg!(debug_assertions));
        std::env::remove_var("OAK_TEST_CRASH_AT");
    }
}
