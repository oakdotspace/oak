//! `Repository` implementation backed by a real `.git` directory.
//!
//! Like jj's git backend, the repository is *colocated*: commits, trees,
//! blobs, branches, and HEAD are ordinary git objects and refs, so `git log`,
//! `git status`, and any git host (GitHub, GitLab, ...) see Oak's work
//! directly. Oak-only state (branch descriptions, parents, open/closed status)
//! lives in a sidecar under `<git_dir>/oak/`. Whenever Oak moves HEAD it also
//! resets git's index to HEAD's tree, so plain `git` commands agree with Oak
//! about what is committed. Remote operations (push / fetch / pull) are
//! implemented in the CLI by delegating to the system `git` binary.
//!
//! Hash semantics on this backend:
//! - `Blob.hash` is the git blob OID.
//! - `Manifest.hash` is the git tree OID.
//! - `Commit.hash` is the git commit OID.
//!
//! That's a deliberate departure from the SQLite backend (which uses BLAKE3
//! everywhere). Storing native git OIDs lets Oak commits round-trip through
//! `git log` and other git tools without translation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{
    Blob, Branch, BranchStatus, Capabilities, ChunkInfo, CloseReason, Commit, FileChange, FileMode,
    Hash, Manifest, ManifestEntry, MetadataKey, OakError, Result, Tag,
};
use gix::bstr::BString;
use gix::ObjectId;

use crate::traits::Repository;

const BACKEND: &str = "git";

/// Read-only `Repository` over an existing git directory.
pub struct GitRepository {
    /// Thread-safe handle; we re-derive a thread-local `gix::Repository` per call.
    inner: gix::ThreadSafeRepository,
    /// Sidecar directory for Oak-only state (workdir lock, branch metadata, etc.).
    /// Lives at `<git_dir>/oak/` so it's hidden from normal git operations.
    oak_sidecar_dir: PathBuf,
    /// In-memory metadata fallback. Phase 1 doesn't persist these (writes are unsupported)
    /// but `set_metadata` for `Head` / `CurrentBranch` is harmless to accept silently
    /// because subsequent reads come from the live git refs anyway.
    scratch_metadata: Mutex<std::collections::HashMap<String, String>>,
}

impl GitRepository {
    /// Open an existing git repository at `git_dir` (the `.git` folder, or a bare repo).
    /// `oak_sidecar_dir` is the directory where Oak-only state files live (created on demand).
    pub fn open(git_dir: &Path, oak_sidecar_dir: &Path) -> Result<Self> {
        // Inject a fallback identity so ref/reflog writes — e.g. moving a
        // branch head after a commit (`set_branch_head`) or repointing HEAD —
        // succeed even when the environment configures no `user.name` /
        // `user.email`, as on CI runners. gix derives the reflog committer
        // from config and errors with "The reflog could not be created or
        // updated" when none is found. These `gitoxide.*Fallback` keys are
        // consulted only when no real committer/author identity is present, so
        // a developer's configured git identity still wins where it exists.
        let options = gix::open::Options::default().config_overrides([
            "gitoxide.committer.nameFallback=Oak",
            "gitoxide.committer.emailFallback=oak@oak.space",
            "gitoxide.author.nameFallback=Oak",
            "gitoxide.author.emailFallback=oak@oak.space",
        ]);
        let inner = gix::ThreadSafeRepository::open_opts(git_dir, options).map_err(map_git_err)?;
        Ok(Self {
            inner,
            oak_sidecar_dir: oak_sidecar_dir.to_path_buf(),
            scratch_metadata: Mutex::new(Default::default()),
        })
    }

    fn repo(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }

    /// Reset git's index to HEAD's tree (a `git reset --mixed` that never
    /// touches the working tree). Oak writes commits and moves refs without
    /// going through git's index, so without this `git status` would report
    /// every freshly committed change as staged-and-reverted. Bare repos and
    /// unborn HEADs have nothing to sync.
    fn sync_index_to_head(&self) -> Result<()> {
        let repo = self.repo();
        if repo.workdir().is_none() {
            return Ok(());
        }
        let tree_id = match repo.head_tree_id() {
            Ok(id) => id.detach(),
            Err(_) => return Ok(()),
        };
        let mut index = repo.index_from_tree(&tree_id).map_err(map_git_err)?;
        index
            .write(gix::index::write::Options::default())
            .map_err(map_git_err)?;
        Ok(())
    }

    /// The short name of the branch HEAD points at, if attached.
    fn head_branch_name(&self) -> Result<Option<String>> {
        let repo = self.repo();
        let head = repo.head().map_err(map_git_err)?;
        Ok(head.referent_name().map(|n| {
            let full = n.as_bstr().to_string();
            full.strip_prefix("refs/heads/")
                .unwrap_or(&full)
                .to_string()
        }))
    }

    /// The commit identity: git's configured `user.name` / `user.email` when
    /// present (so commits pushed to GitHub attribute to the right account),
    /// otherwise Oak's author string.
    fn commit_identity(&self, oak_author: &str) -> (String, String) {
        // Read the identity explicitly rather than via `repo.author()`: gix
        // lets the `gitoxide.author.*Fallback` overrides injected in `open`
        // shadow the user's real `user.name` / `user.email`.
        let repo = self.repo();
        let config = repo.config_snapshot();
        let pick = |env: &str, key: &str| {
            std::env::var(env)
                .ok()
                .or_else(|| config.string(key).map(|v| v.to_string()))
                .filter(|v| !v.trim().is_empty())
        };
        match (
            pick("GIT_AUTHOR_NAME", "user.name"),
            pick("GIT_AUTHOR_EMAIL", "user.email"),
        ) {
            (Some(name), Some(email)) => (name, email),
            (None, Some(email)) => (split_author(oak_author).0, email),
            _ => split_author(oak_author),
        }
    }

    /// Build the git commit message for an Oak checkpoint on `branch`.
    fn checkpoint_message(
        &self,
        branch: &str,
        parent: Option<&Hash>,
        files: &[FileChange],
    ) -> Result<String> {
        let meta = BranchSidecar::load(&self.oak_sidecar_dir)?
            .branches
            .get(branch)
            .cloned();
        let description = meta
            .as_ref()
            .and_then(|m| m.description.clone())
            .filter(|d| !d.trim().is_empty());
        let parent_branch = meta.and_then(|m| m.parent_branch);
        let checkpoint = 1 + parent.map_or(0, |p| self.prior_checkpoints(branch, p));
        let stats: Vec<FileStat> = files.iter().map(|f| self.file_stat(f)).collect();
        Ok(format_checkpoint_message(&CheckpointInfo {
            branch,
            parent_branch: parent_branch.as_deref(),
            checkpoint,
            description: description.as_deref(),
            files: &stats,
        }))
    }

    /// How many consecutive first-parent ancestors (starting at `parent`) are
    /// Oak checkpoints on `branch`, recognized by their `Oak-Branch` trailer.
    /// Merge commits in between are skipped, not counted.
    /// Bounded so a pathological history can't stall a commit.
    fn prior_checkpoints(&self, branch: &str, parent: &Hash) -> usize {
        const MAX_WALK: usize = 10_000;
        let trailer = format!("{TRAILER_BRANCH}: {branch}");
        let repo = self.repo();
        let mut count = 0;
        let mut steps = 0;
        let mut current = Self::oid(parent).ok();
        while let Some(oid) = current {
            steps += 1;
            if steps > MAX_WALK {
                break;
            }
            let Ok(obj) = repo.find_object(oid) else {
                break;
            };
            let Ok(commit) = obj.try_into_commit() else {
                break;
            };
            let Ok(message) = commit.message_raw() else {
                break;
            };
            let is_checkpoint = message.to_string().lines().any(|l| l.trim_end() == trailer);
            // Merge commits from `oak pull` interrupt the run of checkpoints
            // without ending it: step over them along the first parent.
            let is_merge = commit.parent_ids().nth(1).is_some();
            if is_checkpoint {
                count += 1;
            } else if !is_merge {
                break;
            }
            current = commit.parent_ids().next().map(|p| p.detach());
        }
        count
    }

    /// Line counts for one changed file, read from the git objects.
    fn file_stat(&self, change: &FileChange) -> FileStat {
        let read = |hash: &Option<Hash>| -> Option<Vec<u8>> {
            match hash {
                None => Some(Vec::new()),
                Some(h) => self.get_blob(h).ok().flatten().map(|b| b.content),
            }
        };
        let lines = match (read(&change.old_blob_hash), read(&change.new_blob_hash)) {
            (Some(old), Some(new)) => line_stat(&old, &new),
            _ => LineStat::Unknown,
        };
        FileStat {
            change_type: change.change_type,
            path: change.path.clone(),
            old_path: change.old_path.clone(),
            // `oak commit` doesn't always carry modes through; a "modified"
            // file whose content is unchanged can only have changed mode.
            mode_changed: (change.old_mode.is_some()
                && change.new_mode.is_some()
                && change.old_mode != change.new_mode)
                || (change.change_type == crate::ChangeType::Modified
                    && change.old_blob_hash.is_some()
                    && change.old_blob_hash == change.new_blob_hash),
            lines,
        }
    }

    fn oid(hash: &Hash) -> Result<ObjectId> {
        ObjectId::from_hex(hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(hash.as_str().to_string()))
    }

    /// Load a commit by OID and convert it to an `crate::Commit`.
    /// `files` is left empty — callers that need it should compute a tree diff lazily.
    fn load_commit(&self, oid: ObjectId) -> Result<Commit> {
        let repo = self.repo();
        let obj = repo.find_object(oid).map_err(map_git_err)?;
        let git_commit = obj
            .try_into_commit()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let tree_id = git_commit.tree_id().map_err(map_git_err)?.detach();
        let parents: Vec<Hash> = git_commit
            .parent_ids()
            .map(|p| Hash(p.detach().to_string()))
            .collect();

        let author = git_commit
            .author()
            .map(|a| a.name.to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        // Git always has a message string; treat empty/whitespace-only as
        // "no message" to match the new oak model where messages are
        // exclusively for main-branch squash-merges.
        let message = git_commit
            .message_raw()
            .ok()
            .map(|m| m.to_string())
            .filter(|s| !s.trim().is_empty());

        let timestamp = git_commit
            .time()
            .ok()
            .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, 0))
            .unwrap_or_else(chrono::Utc::now);

        // Git commits don't carry a branch name — the caller's context (which ref they
        // walked) is the only signal. Leaving empty here; the `get_commits_for_branch`
        // path stamps the branch name on commits it returns.
        Ok(Commit {
            hash: Hash(oid.to_string()),
            branch_name: String::new(),
            parent_hash: parents.first().cloned(),
            merge_parent_hash: parents.get(1).cloned(),
            manifest_hash: Hash(tree_id.to_string()),
            author,
            message,
            timestamp,
            files: Vec::<FileChange>::new(),
        })
    }
}

// --- Checkpoint commit messages ---------------------------------------------
//
// Oak commits have no message of their own, but on a git backend every commit
// shows up in `git log` and on the host (GitHub's commit list, PR timeline,
// blame). Rather than repeat the branch description on every checkpoint, the
// message describes the checkpoint itself:
//
//     Add greet.txt, update README.md
//
//     Checkpoint 2 on feat (parent: main): 2 files changed, +12 -3
//
//       A  greet.txt  +1
//       M  README.md  +11 -3
//
//     Branch description:
//       Add greeting
//
//     Oak-Branch: feat
//     Oak-Checkpoint: 2
//
// The trailers are machine-readable (`git interpret-trailers`) and let the
// next checkpoint number itself by walking first parents.

const TRAILER_BRANCH: &str = "Oak-Branch";
const TRAILER_CHECKPOINT: &str = "Oak-Checkpoint";
/// Longest subject line before it's shortened (git's conventional limit).
const SUBJECT_MAX: usize = 72;
/// At most this many per-file lines in the body.
const MAX_LISTED_FILES: usize = 50;
/// Files larger than this skip the line diff (reported as "large").
const MAX_STAT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LineStat {
    Text { added: usize, removed: usize },
    Binary,
    Large,
    Unknown,
}

#[derive(Debug, Clone)]
struct FileStat {
    change_type: crate::ChangeType,
    path: String,
    old_path: Option<String>,
    mode_changed: bool,
    lines: LineStat,
}

struct CheckpointInfo<'a> {
    branch: &'a str,
    parent_branch: Option<&'a str>,
    checkpoint: usize,
    description: Option<&'a str>,
    files: &'a [FileStat],
}

fn line_stat(old: &[u8], new: &[u8]) -> LineStat {
    if crate::diff::is_binary(old) || crate::diff::is_binary(new) {
        return LineStat::Binary;
    }
    if old.len() > MAX_STAT_BYTES || new.len() > MAX_STAT_BYTES {
        return LineStat::Large;
    }
    let old = String::from_utf8_lossy(old);
    let new = String::from_utf8_lossy(new);
    let (mut added, mut removed) = (0, 0);
    for change in similar::TextDiff::from_lines(old.as_ref(), new.as_ref()).iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    LineStat::Text { added, removed }
}

fn verb(change: &crate::ChangeType) -> &'static str {
    match change {
        crate::ChangeType::Added => "Add",
        crate::ChangeType::Modified => "Update",
        crate::ChangeType::Deleted => "Delete",
        crate::ChangeType::Renamed => "Rename",
    }
}

/// A path for a subject line: the file name alone when `short` (or when the
/// full path is long), otherwise the full path.
fn subject_path(path: &str, short: bool) -> &str {
    if short || path.len() > 40 {
        path.rsplit('/').next().unwrap_or(path)
    } else {
        path
    }
}

fn subject_for_file(f: &FileStat, short: bool) -> String {
    let path = subject_path(&f.path, short);
    match (&f.change_type, &f.old_path) {
        (crate::ChangeType::Renamed, Some(old)) => {
            format!("Rename {} to {path}", subject_path(old, short))
        }
        (crate::ChangeType::Modified, _)
            if f.mode_changed
                && f.lines
                    == (LineStat::Text {
                        added: 0,
                        removed: 0,
                    }) =>
        {
            format!("Change mode of {path}")
        }
        (ct, _) => format!("{} {path}", verb(ct)),
    }
}

/// The directory containing every path ("" when they share none).
fn common_dir(paths: &[&str]) -> String {
    let mut common: Option<Vec<&str>> = None;
    for path in paths {
        let mut parts: Vec<&str> = path.split('/').collect();
        parts.pop(); // the file name
        common = Some(match common {
            None => parts,
            Some(prev) => prev
                .iter()
                .zip(parts.iter())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    common.unwrap_or_default().join("/")
}

/// "2 added, 3 modified" — omitted kinds are skipped.
fn change_breakdown(files: &[FileStat]) -> String {
    let count = |ct: crate::ChangeType| files.iter().filter(|f| f.change_type == ct).count();
    [
        (count(crate::ChangeType::Added), "added"),
        (count(crate::ChangeType::Modified), "modified"),
        (count(crate::ChangeType::Deleted), "deleted"),
        (count(crate::ChangeType::Renamed), "renamed"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, label)| format!("{n} {label}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn truncate_subject(subject: String) -> String {
    if subject.chars().count() <= SUBJECT_MAX {
        return subject;
    }
    let mut cut: String = subject.chars().take(SUBJECT_MAX - 1).collect();
    cut.push('…');
    cut
}

/// One-line summary of a checkpoint's changes, e.g. `Add a.rs, update b.rs`
/// or `Update 7 files in cli/src (2 added, 5 modified)`.
fn checkpoint_subject(files: &[FileStat], branch: &str) -> String {
    if files.is_empty() {
        return format!("Checkpoint on {branch}");
    }
    // Same kind of change to a few files: say the verb (and directory) once,
    // e.g. `Add m1.rs, m2.rs and m3.rs in src/cmd`.
    let same_kind = files.iter().all(|f| {
        f.change_type == files[0].change_type
            && f.change_type != crate::ChangeType::Renamed
            && !f.mode_changed
    });
    if files.len() > 1 && files.len() <= 3 && same_kind {
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let dir = common_dir(&paths);
        let names: Vec<&str> = paths
            .iter()
            .map(|p| {
                if dir.is_empty() {
                    *p
                } else {
                    &p[dir.len() + 1..]
                }
            })
            .collect();
        let (last, rest) = names.split_last().expect("more than one file");
        let mut subject = format!(
            "{} {} and {last}",
            verb(&files[0].change_type),
            rest.join(", ")
        );
        if !dir.is_empty() {
            subject.push_str(&format!(" in {dir}"));
        }
        if subject.chars().count() <= SUBJECT_MAX {
            return subject;
        }
    }
    if files.len() <= 3 {
        // Full paths first; if that's too long, bare file names.
        for short in [false, true] {
            let parts: Vec<String> = files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let s = subject_for_file(f, short);
                    if i == 0 {
                        s
                    } else {
                        let mut chars = s.chars();
                        chars
                            .next()
                            .map(|c| c.to_lowercase().chain(chars).collect())
                            .unwrap_or_default()
                    }
                })
                .collect();
            let joined = parts.join(", ");
            if joined.chars().count() <= SUBJECT_MAX {
                return joined;
            }
        }
    }

    let first = &files[0].change_type;
    let mixed = files.iter().any(|f| &f.change_type != first);
    let verb = if mixed { "Update" } else { verb(first) };
    let n = files.len();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let dir = common_dir(&paths);
    let mut subject = if !dir.is_empty() {
        format!("{verb} {n} files in {dir}")
    } else {
        let mut tops: Vec<&str> = paths
            .iter()
            .map(|p| match p.split_once('/') {
                Some((top, _)) => top,
                None => ".",
            })
            .collect();
        tops.sort_unstable();
        tops.dedup();
        if tops.len() <= 3 {
            // Directories first, then files at the top level.
            let mut listed: Vec<String> = tops
                .iter()
                .filter(|t| **t != ".")
                .map(|t| format!("{t}/"))
                .collect();
            if tops.contains(&".") {
                listed.push("the top level".to_string());
            }
            let listed = match listed.split_last() {
                Some((last, rest)) if !rest.is_empty() => {
                    format!("{} and {last}", rest.join(", "))
                }
                _ => listed.join(""),
            };
            format!("{verb} {n} files in {listed}")
        } else {
            format!("{verb} {n} files across {} directories", tops.len())
        }
    };
    if mixed {
        let with_breakdown = format!("{subject} ({})", change_breakdown(files));
        if with_breakdown.chars().count() <= SUBJECT_MAX {
            subject = with_breakdown;
        }
    }
    truncate_subject(subject)
}

fn format_line_stat(f: &FileStat) -> String {
    let mut out = match &f.lines {
        LineStat::Text { added, removed } => match (added, removed) {
            (0, 0) => String::new(),
            (a, 0) => format!("+{a}"),
            (0, r) => format!("-{r}"),
            (a, r) => format!("+{a} -{r}"),
        },
        LineStat::Binary => "binary".to_string(),
        LineStat::Large => "large".to_string(),
        LineStat::Unknown => String::new(),
    };
    if f.mode_changed {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str("mode changed");
    }
    out
}

fn format_checkpoint_message(info: &CheckpointInfo<'_>) -> String {
    let files = info.files;
    let mut msg = checkpoint_subject(files, info.branch);
    msg.push_str("\n\n");

    // Summary line: where this checkpoint sits, and how big it is.
    msg.push_str(&format!(
        "Checkpoint {} on {}",
        info.checkpoint, info.branch
    ));
    if let Some(parent) = info.parent_branch.filter(|p| *p != info.branch) {
        msg.push_str(&format!(" (parent: {parent})"));
    }
    if !files.is_empty() {
        let (added, removed) = files.iter().fold((0, 0), |(a, r), f| match f.lines {
            LineStat::Text { added, removed } => (a + added, r + removed),
            _ => (a, r),
        });
        let binary = files
            .iter()
            .filter(|f| matches!(f.lines, LineStat::Binary | LineStat::Large))
            .count();
        msg.push_str(&format!(
            ": {} file{} changed, +{added} -{removed}",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
        ));
        if binary > 0 {
            msg.push_str(&format!(", {binary} binary"));
        }
    }
    msg.push('\n');

    // Per-file listing, aligned like `git diff --stat`.
    if !files.is_empty() {
        msg.push('\n');
        let display: Vec<String> = files
            .iter()
            .take(MAX_LISTED_FILES)
            .map(|f| match (&f.change_type, &f.old_path) {
                (crate::ChangeType::Renamed, Some(old)) => format!("{old} → {}", f.path),
                _ => f.path.clone(),
            })
            .collect();
        let width = display
            .iter()
            .map(|d| d.chars().count())
            .max()
            .unwrap_or(0)
            .min(48);
        for (f, path) in files.iter().zip(&display) {
            let stat = format_line_stat(f);
            let line = format!("  {}  {path:<width$}  {stat}", f.change_type);
            msg.push_str(line.trim_end());
            msg.push('\n');
        }
        if files.len() > MAX_LISTED_FILES {
            msg.push_str(&format!(
                "  … and {} more files\n",
                files.len() - MAX_LISTED_FILES
            ));
        }
    }

    // The branch narrative, for context when reading one commit in isolation.
    if let Some(description) = info.description.map(str::trim).filter(|d| !d.is_empty()) {
        msg.push_str("\nBranch description:\n");
        for line in description.lines() {
            if line.trim().is_empty() {
                msg.push('\n');
            } else {
                msg.push_str("  ");
                msg.push_str(line);
                msg.push('\n');
            }
        }
    }

    msg.push_str(&format!(
        "\n{TRAILER_BRANCH}: {}\n{TRAILER_CHECKPOINT}: {}\n",
        info.branch, info.checkpoint
    ));
    msg
}

fn map_git_err<E: std::fmt::Display>(e: E) -> OakError {
    OakError::Git(e.to_string())
}

fn unsupported(op: &'static str) -> OakError {
    OakError::Unsupported {
        backend: BACKEND,
        op,
    }
}

// --- Branch metadata sidecar ------------------------------------------------
//
// Git refs only carry a name and a commit OID. Oak's `Branch` struct also has a
// description, parent_branch, status (open/closed), and created_at. Rather than
// silently drop those on the git backend, we persist them in a TOML file at
// `<git_dir>/oak/branches.toml` — outside the git object/ref store, but inside
// `.git/` so it lives with the repo. The sidecar is read lazily and written on
// any branch metadata change.

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct BranchSidecar {
    /// Map of branch name → metadata. Branches without a sidecar entry default
    /// to `(description=None, parent_branch=None, status=Open, created_at=now)`.
    #[serde(default)]
    branches: std::collections::BTreeMap<String, BranchMeta>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct BranchMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_branch: Option<String>,
    /// "open" or "closed" — serialized as a string for forward-compat.
    #[serde(default = "default_status")]
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    close_reason: Option<String>,
    /// RFC3339 timestamp; omitted means "unknown" and we fall back to now-ish on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
}

fn default_status() -> String {
    "open".to_string()
}

impl BranchSidecar {
    fn path(sidecar_dir: &Path) -> PathBuf {
        sidecar_dir.join("branches.toml")
    }

    fn load(sidecar_dir: &Path) -> Result<Self> {
        let path = Self::path(sidecar_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        toml::from_str(&raw).map_err(|e| OakError::Config(format!("branches.toml: {e}")))
    }

    fn save(&self, sidecar_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(sidecar_dir)?;
        let raw = toml::to_string_pretty(self)
            .map_err(|e| OakError::Config(format!("serialize branches.toml: {e}")))?;
        std::fs::write(Self::path(sidecar_dir), raw)?;
        Ok(())
    }
}

fn meta_to_branch(name: &str, meta: &BranchMeta) -> Branch {
    let status = BranchStatus::from_db_str(&meta.status);
    let close_reason = meta
        .close_reason
        .as_deref()
        .map(CloseReason::parse)
        .transpose()
        .unwrap_or(None);
    let created_at = meta
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    Branch {
        name: name.to_string(),
        description: meta.description.clone(),
        parent_branch: meta.parent_branch.clone(),
        status,
        close_reason,
        created_at,
    }
}

fn branch_to_meta(branch: &Branch) -> BranchMeta {
    BranchMeta {
        description: branch.description.clone(),
        parent_branch: branch.parent_branch.clone(),
        status: branch.status.as_str().to_string(),
        close_reason: branch.close_reason.as_ref().map(|r| r.as_str().to_string()),
        created_at: Some(branch.created_at.to_rfc3339()),
    }
}

// --- Write-path helpers -----------------------------------------------------

/// Convert an Oak `FileMode` to the matching git tree entry mode.
fn oak_mode_to_git(mode: FileMode) -> gix::objs::tree::EntryMode {
    match mode {
        FileMode::Regular => gix::objs::tree::EntryKind::Blob.into(),
        FileMode::Executable => gix::objs::tree::EntryKind::BlobExecutable.into(),
        FileMode::Symlink => gix::objs::tree::EntryKind::Link.into(),
    }
}

/// Intermediate node used to build a nested git tree from Oak's flat manifest.
enum TreeNode {
    Blob {
        oid: ObjectId,
        mode: FileMode,
    },
    Dir {
        children: BTreeMap<String, TreeNode>,
    },
}

impl TreeNode {
    fn new_dir() -> Self {
        TreeNode::Dir {
            children: BTreeMap::new(),
        }
    }

    /// Insert a flat path (e.g. `"src/foo.rs"`) into the nested tree.
    fn insert(&mut self, path: &str, oid: ObjectId, mode: FileMode) {
        let TreeNode::Dir { children } = self else {
            return; // Shouldn't happen — root is always a Dir.
        };
        let mut parts = path.splitn(2, '/');
        let head = parts.next().unwrap_or("").to_string();
        match parts.next() {
            None => {
                children.insert(head, TreeNode::Blob { oid, mode });
            }
            Some(rest) => {
                let entry = children.entry(head).or_insert_with(TreeNode::new_dir);
                entry.insert(rest, oid, mode);
            }
        }
    }
}

/// Build the byte key git uses to order a tree entry: the name, with a
/// trailing `/` appended for directories. Sorting entries by this key yields
/// git's canonical (directory-aware) tree order — the order gix's tree writer
/// asserts but does not enforce.
fn git_tree_sort_key(name: &str, is_dir: bool) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    if is_dir {
        key.push(b'/');
    }
    key
}

fn validate_git_tree_entries(entries: &[ManifestEntry]) -> Result<()> {
    for entry in entries {
        crate::validate_tree_path(&entry.path)?;
    }
    Ok(())
}

/// Recursively write nested `TreeNode`s as git trees, returning the root tree OID.
fn write_tree_recursive(repo: &gix::Repository, node: &TreeNode) -> Result<ObjectId> {
    let TreeNode::Dir { children } = node else {
        return Err(OakError::Git(
            "internal error: tried to write a Blob node as a tree".into(),
        ));
    };

    // Git's canonical tree order is directory-aware: an entry sorts as if a
    // tree's name had a trailing '/'. `children` is a `BTreeMap`, so iterating
    // it yields plain lexicographic order — which differs whenever a file and a
    // directory straddle the '/' (0x2f) boundary (classic case: a file `foo`
    // next to a directory `foo`). gix does NOT re-sort: `Tree::write_to` only
    // `debug_assert!`s the entries are already ordered, so the wrong order
    // panics in debug builds and writes a `git fsck`-rejected tree in release.
    // Sort the names ourselves with the trailing-'/' rule before building.
    let mut names: Vec<&String> = children.keys().collect();
    names.sort_by(|a, b| {
        git_tree_sort_key(a, matches!(children[*a], TreeNode::Dir { .. })).cmp(&git_tree_sort_key(
            b,
            matches!(children[*b], TreeNode::Dir { .. }),
        ))
    });

    let mut entries = Vec::with_capacity(children.len());
    for name in names {
        match &children[name] {
            TreeNode::Blob { oid, mode } => {
                entries.push(gix::objs::tree::Entry {
                    mode: oak_mode_to_git(*mode),
                    filename: name.as_str().into(),
                    oid: *oid,
                });
            }
            child @ TreeNode::Dir { .. } => {
                let subtree = write_tree_recursive(repo, child)?;
                entries.push(gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: name.as_str().into(),
                    oid: subtree,
                });
            }
        }
    }

    let tree = gix::objs::Tree { entries };
    let id = repo.write_object(&tree).map_err(map_git_err)?;
    Ok(id.detach())
}

/// Parse an `author` string of the form `"Name <email>"` into separate name/email parts.
/// Falls back to a synthesized email if the input doesn't carry one.
fn split_author(author: &str) -> (String, String) {
    if let Some(start) = author.find('<') {
        if let Some(end) = author[start + 1..].find('>') {
            let name = author[..start].trim().to_string();
            let email = author[start + 1..start + 1 + end].trim().to_string();
            return (name, email);
        }
    }
    (author.to_string(), "oak@oak.space".to_string())
}

/// Walk a git tree recursively into a flat `Vec<ManifestEntry>`.
/// Submodules and other non-blob entries are skipped.
fn read_tree_recursive(
    repo: &gix::Repository,
    tree_id: ObjectId,
    prefix: &str,
    out: &mut Vec<ManifestEntry>,
) -> Result<()> {
    let obj = repo.find_object(tree_id).map_err(map_git_err)?;
    let tree = obj
        .try_into_tree()
        .map_err(|e| OakError::Git(e.to_string()))?;

    for entry in tree.iter() {
        let entry = entry.map_err(map_git_err)?;
        let name = entry.filename().to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };

        let mode = entry.mode();
        if mode.is_tree() {
            read_tree_recursive(repo, entry.id().detach(), &path, out)?;
        } else if mode.is_blob() || mode.is_executable() || mode.is_link() {
            let oak_mode = if mode.is_executable() {
                FileMode::Executable
            } else if mode.is_link() {
                FileMode::Symlink
            } else {
                FileMode::Regular
            };
            out.push(ManifestEntry {
                path,
                blob_hash: Hash(entry.id().detach().to_string()),
                mode: oak_mode,
            });
        }
        // Submodules (gitlinks) and other entry kinds are intentionally dropped.
    }
    Ok(())
}

impl Repository for GitRepository {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            branch_parents: true, // stored in the sidecar
            tags: true,
            releases: false,
            chunks: false,
            branch_metadata: true, // stored in the sidecar
        }
    }

    // --- Blobs ---

    /// `store_blob` is unsupported because Oak's `Blob` carries a precomputed BLAKE3
    /// hash, and writing it as a git blob would assign a different OID. Callers must
    /// use [`Repository::put_blob`] instead, which returns the canonical git OID.
    fn store_blob(&self, _blob: &Blob) -> Result<()> {
        Err(unsupported("store_blob (use put_blob)"))
    }

    fn hash_blob_content(&self, content: &[u8]) -> Hash {
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Blob, content)
            .expect("sha1 hashing of an in-memory buffer cannot fail");
        Hash(oid.to_string())
    }

    fn put_blob(&self, content: Vec<u8>) -> Result<Hash> {
        let repo = self.repo();
        let id = repo.write_blob(content).map_err(map_git_err)?;
        Ok(Hash(id.detach().to_string()))
    }

    fn get_blob(&self, hash: &Hash) -> Result<Option<Blob>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        match obj {
            Some(obj) => {
                let content = obj.data.to_vec();
                let size = content.len() as u64;
                Ok(Some(Blob {
                    hash: hash.clone(),
                    content,
                    size,
                }))
            }
            None => Ok(None),
        }
    }

    fn has_blob(&self, hash: &Hash) -> Result<bool> {
        let oid = Self::oid(hash)?;
        Ok(self
            .repo()
            .try_find_object(oid)
            .map_err(map_git_err)?
            .is_some())
    }

    // --- Manifests (git trees) ---

    /// Unsupported for the same reason as `store_blob` — Oak's manifest hash is
    /// BLAKE3 over a flat encoding, but git trees are nested with their own OID
    /// derivation. Callers must use [`Repository::put_manifest`].
    fn store_manifest(&self, _manifest: &Manifest) -> Result<()> {
        Err(unsupported("store_manifest (use put_manifest)"))
    }

    fn put_manifest(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        validate_git_tree_entries(&entries)?;
        let repo = self.repo();
        let mut root = TreeNode::new_dir();
        for entry in entries {
            let oid = ObjectId::from_hex(entry.blob_hash.as_str().as_bytes())
                .map_err(|_| OakError::InvalidHash(entry.blob_hash.as_str().to_string()))?;
            root.insert(&entry.path, oid, entry.mode);
        }
        let tree_oid = write_tree_recursive(&repo, &root)?;
        Ok(Hash(tree_oid.to_string()))
    }

    fn get_manifest(&self, hash: &Hash) -> Result<Option<Manifest>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        let is_tree = matches!(&obj, Some(o) if o.kind == gix::object::Kind::Tree);
        drop(obj);
        if !is_tree {
            return Ok(None);
        }
        let mut entries = Vec::new();
        read_tree_recursive(&repo, oid, "", &mut entries)?;
        Ok(Some(Manifest {
            hash: hash.clone(),
            entries,
        }))
    }

    // --- Trees (git's native tree objects) ---

    /// Read the direct children of one git tree as an Oak `Tree`.
    /// Submodules are skipped (matching `read_tree_recursive`).
    fn get_tree(&self, hash: &Hash) -> Result<Option<crate::Tree>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = match repo.try_find_object(oid).map_err(map_git_err)? {
            Some(o) if o.kind == gix::object::Kind::Tree => o,
            _ => return Ok(None),
        };
        let tree = obj
            .try_into_tree()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let mut entries = Vec::new();
        for entry in tree.iter() {
            let entry = entry.map_err(map_git_err)?;
            let mode = entry.mode();
            if mode.is_tree() {
                entries.push(crate::TreeEntry {
                    name: entry.filename().to_string(),
                    kind: crate::TreeEntryKind::Tree,
                    hash: Hash(entry.id().detach().to_string()),
                    mode: FileMode::Regular,
                });
            } else if mode.is_blob() || mode.is_executable() || mode.is_link() {
                let oak_mode = if mode.is_executable() {
                    FileMode::Executable
                } else if mode.is_link() {
                    FileMode::Symlink
                } else {
                    FileMode::Regular
                };
                entries.push(crate::TreeEntry {
                    name: entry.filename().to_string(),
                    kind: crate::TreeEntryKind::Blob,
                    hash: Hash(entry.id().detach().to_string()),
                    mode: oak_mode,
                });
            }
        }
        Ok(Some(crate::Tree {
            hash: hash.clone(),
            entries,
        }))
    }

    /// Unsupported for the same reason as `store_blob`: the Tree's hash is Oak's
    /// BLAKE3, but a git tree object would assign its own OID. Use [`put_tree`]
    /// instead, which returns the git OID.
    fn store_tree(&self, _tree: &crate::Tree) -> Result<()> {
        Err(unsupported("store_tree (use put_tree)"))
    }

    fn put_tree(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        // Same algorithm as `put_manifest`: build a nested TreeNode and write
        // it as git trees. Returns the root tree OID.
        validate_git_tree_entries(&entries)?;
        let repo = self.repo();
        let mut root = TreeNode::new_dir();
        for entry in entries {
            let oid = ObjectId::from_hex(entry.blob_hash.as_str().as_bytes())
                .map_err(|_| OakError::InvalidHash(entry.blob_hash.as_str().to_string()))?;
            root.insert(&entry.path, oid, entry.mode);
        }
        let tree_oid = write_tree_recursive(&repo, &root)?;
        Ok(Hash(tree_oid.to_string()))
    }

    fn walk_tree(&self, root: &Hash) -> Result<Vec<ManifestEntry>> {
        let oid = Self::oid(root)?;
        let repo = self.repo();
        let mut entries = Vec::new();
        read_tree_recursive(&repo, oid, "", &mut entries)?;
        Ok(entries)
    }

    // --- Branches (git refs) ---

    /// Create the git ref for a new branch, pointing at the current HEAD, and
    /// record Oak's branch metadata (description / parent_branch / status / created_at)
    /// in the sidecar TOML at `<git_dir>/oak/branches.toml`.
    ///
    /// The git ref is what actually makes the branch usable; the sidecar just
    /// preserves Oak-only fields so `oak branch show` / `branch list` aren't lossy.
    fn store_branch(&self, branch: &Branch) -> Result<()> {
        let repo = self.repo();
        let refname = format!("refs/heads/{}", branch.name);

        // Create the ref if missing (don't move it if it already exists).
        if repo
            .try_find_reference(&refname)
            .map_err(map_git_err)?
            .is_none()
        {
            let head = repo.head().map_err(map_git_err)?;
            let head_oid = match head.try_into_peeled_id() {
                Ok(Some(id)) => id.detach(),
                Ok(None) => {
                    return Err(OakError::Git(format!(
                        "cannot create branch '{}': repository has no commits yet",
                        branch.name
                    )));
                }
                Err(e) => return Err(map_git_err(e)),
            };

            repo.reference(
                refname.as_str(),
                head_oid,
                gix::refs::transaction::PreviousValue::MustNotExist,
                BString::from(format!("oak: create branch {}", branch.name)),
            )
            .map_err(map_git_err)?;
        }

        // Persist Oak-only metadata in the sidecar.
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        sidecar
            .branches
            .insert(branch.name.clone(), branch_to_meta(branch));
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn get_branch(&self, name: &str) -> Result<Option<Branch>> {
        let repo = self.repo();
        let refname = format!("refs/heads/{name}");
        // HEAD's branch in a fresh repo is "unborn": HEAD names it but the ref
        // doesn't exist until the first commit. It's still the branch the user
        // is on (and the one `oak commit` creates), so report it.
        if repo
            .try_find_reference(&refname)
            .map_err(map_git_err)?
            .is_none()
            && self.head_branch_name()?.as_deref() != Some(name)
        {
            return Ok(None);
        }
        let sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let branch = match sidecar.branches.get(name) {
            Some(meta) => meta_to_branch(name, meta),
            None => Branch {
                name: name.to_string(),
                description: None,
                parent_branch: None,
                status: BranchStatus::Open,
                close_reason: None,
                created_at: chrono::Utc::now(),
            },
        };
        Ok(Some(branch))
    }

    fn list_branches(&self) -> Result<Vec<Branch>> {
        let repo = self.repo();
        let platform = repo.references().map_err(map_git_err)?;
        let iter = platform.local_branches().map_err(map_git_err)?;
        let sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let mut out = Vec::new();
        for r in iter {
            let r = r.map_err(map_git_err)?;
            let full = r.name().as_bstr().to_string();
            let name = full
                .strip_prefix("refs/heads/")
                .unwrap_or(&full)
                .to_string();
            let branch = match sidecar.branches.get(&name) {
                Some(meta) => meta_to_branch(&name, meta),
                None => Branch {
                    name,
                    description: None,
                    parent_branch: None,
                    status: BranchStatus::Open,
                    close_reason: None,
                    created_at: chrono::Utc::now(),
                },
            };
            out.push(branch);
        }
        Ok(out)
    }

    fn update_branch_status(&self, name: &str, status: BranchStatus) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.status = status.as_str().to_string();
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn close_branch(&self, name: &str, close_reason: Option<CloseReason>) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.status = BranchStatus::Closed.as_str().to_string();
        entry.close_reason = close_reason.map(|r| r.as_str().to_string());
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn update_branch_close_reason(&self, name: &str, close_reason: CloseReason) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.close_reason = Some(close_reason.as_str().to_string());
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn update_branch_description(&self, name: &str, description: &str) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.description = if description.is_empty() {
            None
        } else {
            Some(description.to_string())
        };
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn get_branch_head(&self, name: &str) -> Result<Option<Hash>> {
        let repo = self.repo();
        let refname = format!("refs/heads/{name}");
        let r = repo.try_find_reference(&refname).map_err(map_git_err)?;
        match r {
            Some(mut r) => {
                let id = r.peel_to_id().map_err(map_git_err)?;
                Ok(Some(Hash(id.detach().to_string())))
            }
            None => Ok(None),
        }
    }

    fn set_branch_head(&self, name: &str, hash: &Hash) -> Result<()> {
        let repo = self.repo();
        let oid = ObjectId::from_hex(hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(hash.as_str().to_string()))?;
        let refname = format!("refs/heads/{name}");
        repo.reference(
            refname.as_str(),
            oid,
            gix::refs::transaction::PreviousValue::Any,
            BString::from(format!("oak: set head of {name}")),
        )
        .map_err(map_git_err)?;
        if self.head_branch_name()?.as_deref() == Some(name) {
            self.sync_index_to_head()?;
        }
        Ok(())
    }

    // --- Commits ---

    fn store_commit(&self, _commit: &Commit) -> Result<()> {
        Err(unsupported("store_commit (use put_commit)"))
    }

    fn put_commit(
        &self,
        branch_name: String,
        parent_hash: Option<Hash>,
        merge_parent_hash: Option<Hash>,
        manifest_hash: Hash,
        author: String,
        message: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
        files: Vec<FileChange>,
    ) -> Result<Hash> {
        // Oak checkpoints carry no message; the branch description is the
        // narrative. Git (and every git host) shows one per commit, so
        // generate it from what this checkpoint actually changed — see
        // `format_checkpoint_message`.
        let message = match message {
            Some(m) => m,
            None => self.checkpoint_message(&branch_name, parent_hash.as_ref(), &files)?,
        };
        let repo = self.repo();
        let tree = ObjectId::from_hex(manifest_hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(manifest_hash.as_str().to_string()))?;

        let mut parents: Vec<ObjectId> = Vec::new();
        if let Some(p) = parent_hash {
            parents.push(
                ObjectId::from_hex(p.as_str().as_bytes())
                    .map_err(|_| OakError::InvalidHash(p.as_str().to_string()))?,
            );
        }
        if let Some(p) = merge_parent_hash {
            parents.push(
                ObjectId::from_hex(p.as_str().as_bytes())
                    .map_err(|_| OakError::InvalidHash(p.as_str().to_string()))?,
            );
        }

        let (name, email) = self.commit_identity(&author);
        let sig = gix::actor::Signature {
            name: name.into(),
            email: email.into(),
            time: gix::date::Time::new(timestamp.timestamp(), 0),
        };

        let commit = gix::objs::Commit {
            tree,
            parents: parents.into(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: message.into(),
            extra_headers: vec![],
        };

        let id = repo.write_object(&commit).map_err(map_git_err)?;
        Ok(Hash(id.detach().to_string()))
    }

    fn get_commit(&self, hash: &Hash) -> Result<Option<Commit>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        let is_commit = matches!(&obj, Some(o) if o.kind == gix::object::Kind::Commit);
        drop(obj);
        drop(repo);
        if !is_commit {
            return Ok(None);
        }
        Ok(Some(self.load_commit(oid)?))
    }

    fn has_commit(&self, hash: &Hash) -> Result<bool> {
        let oid = Self::oid(hash)?;
        Ok(self
            .repo()
            .try_find_object(oid)
            .map_err(map_git_err)?
            .is_some())
    }

    fn get_commits_for_branch(&self, branch_name: &str) -> Result<Vec<Commit>> {
        let head = match self.get_branch_head(branch_name)? {
            Some(h) => h,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut current = Some(Self::oid(&head)?);
        while let Some(oid) = current {
            let mut commit = self.load_commit(oid)?;
            // Branch name isn't stored on git commits — stamp it from the caller's context.
            commit.branch_name = branch_name.to_string();
            current = commit.parent_hash.as_ref().and_then(|p| Self::oid(p).ok());
            out.push(commit);
        }
        Ok(out)
    }

    fn get_commits_since(&self, branch_name: &str, since: Option<&Hash>) -> Result<Vec<Commit>> {
        let all = self.get_commits_for_branch(branch_name)?;
        match since {
            None => Ok(all),
            Some(stop) => Ok(all.into_iter().take_while(|c| &c.hash != stop).collect()),
        }
    }

    fn get_all_commits(&self) -> Result<Vec<Commit>> {
        // Walk every local branch tip; dedupe by hash. For huge repos this is expensive
        // but matches the SQLite backend's semantics ("everything reachable").
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for branch in self.list_branches()? {
            for commit in self.get_commits_for_branch(&branch.name)? {
                if seen.insert(commit.hash.clone()) {
                    out.push(commit);
                }
            }
        }
        Ok(out)
    }

    // --- Metadata ---

    fn get_metadata(&self, key: MetadataKey) -> Result<Option<String>> {
        match key {
            MetadataKey::Head => {
                // Resolve HEAD to a commit OID (works for both attached and detached HEAD).
                let repo = self.repo();
                match repo.head().map_err(map_git_err)?.try_into_peeled_id() {
                    Ok(Some(id)) => Ok(Some(id.detach().to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(map_git_err(e)),
                }
            }
            MetadataKey::CurrentBranch => {
                // Symbolic HEAD → return the branch short name.
                let repo = self.repo();
                let head = repo.head().map_err(map_git_err)?;
                Ok(head.referent_name().map(|n| {
                    let full = n.as_bstr().to_string();
                    full.strip_prefix("refs/heads/")
                        .unwrap_or(&full)
                        .to_string()
                }))
            }
            _ => Ok(self
                .scratch_metadata
                .lock()
                .unwrap()
                .get(key.as_str())
                .cloned()),
        }
    }

    fn set_metadata(&self, key: MetadataKey, value: &str) -> Result<()> {
        match key {
            MetadataKey::Head => {
                // Oak's `set_head` is called after every commit to record the new tip.
                // For attached HEAD, gix updates the underlying branch ref automatically
                // when `set_branch_head` is called, so this is usually redundant. We only
                // need to act when HEAD is detached — write the OID directly.
                let repo = self.repo();
                let oid = ObjectId::from_hex(value.as_bytes())
                    .map_err(|_| OakError::InvalidHash(value.to_string()))?;
                let head = repo.head().map_err(map_git_err)?;
                if head.referent_name().is_none() {
                    // Detached HEAD — point HEAD directly at the OID.
                    repo.reference(
                        "HEAD",
                        oid,
                        gix::refs::transaction::PreviousValue::Any,
                        BString::from("oak: set detached HEAD"),
                    )
                    .map_err(map_git_err)?;
                    self.sync_index_to_head()?;
                }
                Ok(())
            }
            MetadataKey::CurrentBranch => {
                // Update HEAD to be a symbolic ref pointing at refs/heads/<value>.
                // gix doesn't expose a one-line "make HEAD symbolic" — open the ref
                // file directly via the loose store.
                let repo = self.repo();
                let target = format!("refs/heads/{value}");
                let target_name: gix::refs::FullName =
                    target.as_str().try_into().map_err(map_git_err)?;
                let edit = gix::refs::transaction::RefEdit {
                    change: gix::refs::transaction::Change::Update {
                        log: gix::refs::transaction::LogChange {
                            mode: gix::refs::transaction::RefLog::AndReference,
                            force_create_reflog: false,
                            message: BString::from(format!("oak: switch HEAD to {value}")),
                        },
                        expected: gix::refs::transaction::PreviousValue::Any,
                        new: gix::refs::Target::Symbolic(target_name),
                    },
                    name: "HEAD".try_into().map_err(map_git_err)?,
                    deref: false,
                };
                repo.edit_reference(edit).map_err(map_git_err)?;
                drop(repo);
                self.sync_index_to_head()?;
                Ok(())
            }
            _ => {
                self.scratch_metadata
                    .lock()
                    .unwrap()
                    .insert(key.as_str().to_string(), value.to_string());
                Ok(())
            }
        }
    }

    // --- Tags ---

    /// Tags are lightweight git tags (`refs/tags/<name>`), so `git push --tags`
    /// and git hosts see them as-is.
    fn create_tag(&self, name: &str, commit_hash: &Hash) -> Result<()> {
        let oid = Self::oid(commit_hash)?;
        self.repo()
            .reference(
                format!("refs/tags/{name}").as_str(),
                oid,
                gix::refs::transaction::PreviousValue::MustNotExist,
                BString::from(format!("oak: create tag {name}")),
            )
            .map_err(map_git_err)?;
        Ok(())
    }

    fn list_tags(&self) -> Result<Vec<Tag>> {
        let repo = self.repo();
        let platform = repo.references().map_err(map_git_err)?;
        let iter = platform.tags().map_err(map_git_err)?;
        let mut out = Vec::new();
        for r in iter {
            let mut r = r.map_err(map_git_err)?;
            let full = r.name().as_bstr().to_string();
            let name = full.strip_prefix("refs/tags/").unwrap_or(&full).to_string();
            // Annotated tags peel through a tag object; lightweight tags point straight at a commit.
            let commit_id = r.peel_to_id().map_err(map_git_err)?.detach();
            out.push(Tag {
                name,
                commit_hash: Hash(commit_id.to_string()),
                created_at: chrono::Utc::now(),
            });
        }
        Ok(out)
    }

    fn delete_tag(&self, name: &str) -> Result<()> {
        let repo = self.repo();
        if let Some(r) = repo
            .try_find_reference(format!("refs/tags/{name}").as_str())
            .map_err(map_git_err)?
        {
            r.delete().map_err(map_git_err)?;
        }
        Ok(())
    }

    // --- Chunks ---

    fn store_chunk(&self, _hash: &Hash, _content: &[u8]) -> Result<()> {
        Err(unsupported("store_chunk"))
    }
    fn get_chunk(&self, _hash: &Hash) -> Result<Option<Vec<u8>>> {
        Err(unsupported("get_chunk"))
    }
    fn has_chunk(&self, _hash: &Hash) -> Result<bool> {
        Err(unsupported("has_chunk"))
    }
    fn store_blob_chunks(&self, _blob_hash: &Hash, _chunks: &[ChunkInfo]) -> Result<()> {
        Err(unsupported("store_blob_chunks"))
    }
    fn get_blob_chunks(&self, _blob_hash: &Hash) -> Result<Option<Vec<ChunkInfo>>> {
        Err(unsupported("get_blob_chunks"))
    }
}

#[allow(dead_code)]
fn _silence_unused_field(g: &GitRepository) -> &Path {
    // `oak_sidecar_dir` is held for Phase 2 (branch metadata sidecar, etc.).
    &g.oak_sidecar_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file `foo.txt` next to a directory `foo` is the canonical case where
    /// plain lexicographic order (`foo` < `foo.txt`) diverges from git's
    /// directory-aware order (`foo.txt` < `foo/`, because '.' 0x2e < '/' 0x2f).
    /// Getting this wrong panics gix's tree writer in debug and produces a
    /// `git fsck`-rejected tree in release.
    #[test]
    fn dir_and_file_straddling_slash_sort_in_git_order() {
        // Start in the "wrong" (BTreeMap) order to prove the sort reorders.
        let mut names = [("foo", true), ("foo.txt", false)];
        names.sort_by(|(a, ad), (b, bd)| git_tree_sort_key(a, *ad).cmp(&git_tree_sort_key(b, *bd)));
        assert_eq!(
            names.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec!["foo.txt", "foo"],
            "git orders the file before the same-stem directory"
        );
    }

    #[test]
    fn two_dirs_and_two_files_sort_stably() {
        let mut names = [("b", true), ("a.txt", false), ("a", true), ("b.txt", false)];
        names.sort_by(|(a, ad), (b, bd)| git_tree_sort_key(a, *ad).cmp(&git_tree_sort_key(b, *bd)));
        // a/, then a.txt? No: "a/" (0x2f) vs "a.txt" (0x2e) → "a.txt" < "a/".
        assert_eq!(
            names.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec!["a.txt", "a", "b.txt", "b"],
        );
    }

    fn stat(ct: crate::ChangeType, path: &str, added: usize, removed: usize) -> FileStat {
        FileStat {
            change_type: ct,
            path: path.to_string(),
            old_path: None,
            mode_changed: false,
            lines: LineStat::Text { added, removed },
        }
    }

    #[test]
    fn subject_lists_up_to_three_files_with_lowercased_verbs() {
        let files = [
            stat(crate::ChangeType::Modified, "README.md", 2, 1),
            stat(crate::ChangeType::Added, "greet.txt", 1, 0),
        ];
        assert_eq!(
            checkpoint_subject(&files, "feat"),
            "Update README.md, add greet.txt"
        );
    }

    #[test]
    fn subject_falls_back_to_file_names_then_directory_summary() {
        let long = |name: &str| format!("some/deeply/nested/directory/tree/{name}");
        let files = [
            stat(crate::ChangeType::Modified, &long("alpha_module.rs"), 1, 1),
            stat(crate::ChangeType::Modified, &long("beta_module.rs"), 1, 1),
        ];
        assert_eq!(
            checkpoint_subject(&files, "feat"),
            "Update alpha_module.rs, update beta_module.rs"
        );

        let many: Vec<FileStat> = (0..5)
            .map(|i| stat(crate::ChangeType::Added, &format!("src/cmd/m{i}.rs"), 1, 0))
            .collect();
        assert_eq!(checkpoint_subject(&many, "feat"), "Add 5 files in src/cmd");

        let mut mixed = many.clone();
        mixed.push(stat(crate::ChangeType::Deleted, "README.md", 0, 3));
        assert_eq!(
            checkpoint_subject(&mixed, "feat"),
            "Update 6 files in src/ and the top level (5 added, 1 deleted)"
        );
    }

    #[test]
    fn subject_states_a_shared_verb_and_directory_once() {
        let files: Vec<FileStat> = (1..=3)
            .map(|i| stat(crate::ChangeType::Added, &format!("src/cmd/m{i}.rs"), 1, 0))
            .collect();
        assert_eq!(
            checkpoint_subject(&files, "feat"),
            "Add m1.rs, m2.rs and m3.rs in src/cmd"
        );
        let top = [
            stat(crate::ChangeType::Deleted, "a.txt", 0, 1),
            stat(crate::ChangeType::Deleted, "b.txt", 0, 1),
        ];
        assert_eq!(checkpoint_subject(&top, "feat"), "Delete a.txt and b.txt");
    }

    #[test]
    fn subject_names_renames_and_mode_changes() {
        let mut rename = stat(crate::ChangeType::Renamed, "b.txt", 0, 0);
        rename.old_path = Some("a.txt".to_string());
        let mut chmod = stat(crate::ChangeType::Modified, "run.sh", 0, 0);
        chmod.mode_changed = true;
        assert_eq!(
            checkpoint_subject(&[rename, chmod], "feat"),
            "Rename a.txt to b.txt, change mode of run.sh"
        );
        assert_eq!(checkpoint_subject(&[], "feat"), "Checkpoint on feat");
    }

    #[test]
    fn long_subjects_are_truncated_to_the_git_limit() {
        let subject = truncate_subject("x".repeat(100));
        assert_eq!(subject.chars().count(), SUBJECT_MAX);
        assert!(subject.ends_with('…'));
    }

    #[test]
    fn line_stat_counts_lines_and_flags_binary() {
        assert_eq!(
            line_stat(b"a\nb\n", b"a\nc\nd\n"),
            LineStat::Text {
                added: 2,
                removed: 1
            }
        );
        assert_eq!(line_stat(b"", b"\0\x01"), LineStat::Binary);
    }

    #[test]
    fn message_has_summary_file_table_description_and_trailers() {
        let files = [
            stat(crate::ChangeType::Modified, "README.md", 2, 1),
            stat(crate::ChangeType::Added, "greet.txt", 1, 0),
        ];
        let msg = format_checkpoint_message(&CheckpointInfo {
            branch: "feat",
            parent_branch: Some("main"),
            checkpoint: 2,
            description: Some("Add greeting\n\nDetails here."),
            files: &files,
        });
        assert_eq!(
            msg,
            "Update README.md, add greet.txt\n\
             \n\
             Checkpoint 2 on feat (parent: main): 2 files changed, +3 -1\n\
             \n\
             \x20 M  README.md  +2 -1\n\
             \x20 A  greet.txt  +1\n\
             \n\
             Branch description:\n\
             \x20 Add greeting\n\
             \n\
             \x20 Details here.\n\
             \n\
             Oak-Branch: feat\n\
             Oak-Checkpoint: 2\n"
        );
    }

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: Hash::from_hex(&"a".repeat(40)).unwrap(),
            mode: FileMode::Regular,
        }
    }

    #[test]
    fn validate_git_tree_entries_accepts_normal_paths() {
        validate_git_tree_entries(&[entry("src/main.rs"), entry("README.md")]).unwrap();
    }

    #[test]
    fn validate_git_tree_entries_rejects_invalid_components() {
        for path in ["../escape.txt", "src/../escape.txt", "bad\nname.txt"] {
            let err = validate_git_tree_entries(&[entry(path)]).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidPath(_)),
                "{path:?} should be InvalidPath, got {err:?}"
            );
        }
    }
}
