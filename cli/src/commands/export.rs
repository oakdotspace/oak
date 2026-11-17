//! `oak export` — replay an oak branch's history into a git repository.
//!
//! The inverse of `oak clone <git-url>` / `oak init --import-git`, and Oak's
//! escape hatch: code and history are never trapped.
//!
//! The history is streamed into `git fast-import` rather than replayed through
//! a working tree. That keeps every file exactly as Oak stores it: nothing a
//! working-tree replay would apply gets a say — not `.gitignore` (a tracked
//! file that an ignore pattern matches would silently drop out of `git add`),
//! not `.gitattributes` filters or line-ending conversion, not the user's
//! hooks or signing config. It is also one pass over the history instead of
//! rewriting every file at every commit.
//!
//! What comes across:
//!   - Every commit on the branch's first-parent line, oldest first, with its
//!     author, message and timestamp (as both author and committer, so the
//!     same history always exports to the same git commit ids).
//!   - Merge parents that belong to the same branch, which is what an
//!     imported git merge looks like. A squash-merge's link to the branch it
//!     merged (a commit on another branch) is left out, so `main` exports as
//!     the straight line of squash commits it is.
//!   - File modes: regular, executable and symlink.
//!
//! What can't come across is reported, not guessed at: a commit whose parent
//! isn't stored locally (a shallow clone) starts the git history there, and a
//! blob that isn't stored locally (sparse or restricted) fails the export
//! with the path that needs it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::sync::Arc;

use indicatif::{ProgressBar, ProgressStyle};
use oak_core::{Commit, FileMode, Hash, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// What `oak export` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Git commits written.
    pub commits: usize,
    /// The git branch they're on.
    pub git_branch: String,
    /// Same-branch merge parents carried across.
    pub merges: usize,
    /// Commits whose parent isn't stored locally, so their git commit has
    /// none (a shallow clone's oldest commits).
    pub cut_parents: usize,
    /// Whether the destination was a new repository whose working tree was
    /// checked out from the exported branch.
    pub checked_out: bool,
}

pub fn run(
    cwd: &Path,
    dest: &Path,
    branch_override: Option<&str>,
    branch_name_override: Option<&str>,
    force: bool,
) -> Result<()> {
    let outcome = export(cwd, dest, branch_override, branch_name_override, force)?;
    let dest = absolute(cwd, dest);
    output::success(&format!(
        "Exported {} commit(s) to git branch '{}' at {}",
        outcome.commits,
        outcome.git_branch,
        dest.display(),
    ));
    if outcome.merges > 0 {
        output::item(&format!(
            "{} merge commit(s) kept both parents",
            outcome.merges
        ));
    }
    if outcome.cut_parents > 0 {
        output::warning(&format!(
            "{} commit(s) have a parent this clone doesn't store, so the git history starts there. Export from a full clone (`oak clone` without --shallow) for all of it.",
            outcome.cut_parents
        ));
    }
    if !outcome.checked_out {
        output::info(&format!(
            "The working tree in {} was left as it was; check out '{}' there to see the files.",
            dest.display(),
            outcome.git_branch
        ));
    }
    Ok(())
}

fn absolute(cwd: &Path, path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// [`run`] without the narration.
pub fn export(
    cwd: &Path,
    dest: &Path,
    branch_override: Option<&str>,
    branch_name_override: Option<&str>,
    force: bool,
) -> Result<ExportOutcome> {
    let ctx = crate::resolve::resolve(cwd)?;
    let oak_repo = SqliteRepository::open(&ctx.db_path()?)?;

    let branch_name = match branch_override {
        Some(n) => n.to_string(),
        None => oak_repo.get_current_branch_name()?.ok_or_else(|| {
            OakError::InvalidArgument(
                "No current branch set; pass --branch <NAME> to choose one explicitly.".into(),
            )
        })?,
    };
    let head = oak_repo.get_branch_head(&branch_name)?.ok_or_else(|| {
        OakError::InvalidArgument(format!("Branch '{branch_name}' has no commits to export"))
    })?;
    let git_branch = branch_name_override.unwrap_or(&branch_name).to_string();
    check_git_branch_name(&git_branch)?;

    let history = collect_history(&oak_repo, &head)?;

    // Destination: a new repository in an absent or empty directory, or
    // (with --force) anything else, including an existing git repository,
    // whose branch is then overwritten and whose working tree is left alone.
    let dest = absolute(cwd, dest);
    let non_empty = dest
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if non_empty && !force {
        return Err(OakError::InvalidArgument(format!(
            "Destination '{}' already exists and is not empty (pass --force to write into it anyway)",
            dest.display()
        )));
    }
    let fresh = !non_empty;
    if !dest.join(".git").exists() {
        fs::create_dir_all(&dest)?;
        run_git(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(format!("--initial-branch={git_branch}"))
                .arg("--")
                .arg(&dest),
            "git init",
        )?;
    }

    output::info(&format!(
        "Exporting {} commit(s) from oak branch '{}' to {}...",
        history.order.len(),
        branch_name,
        dest.display()
    ));
    stream_to_fast_import(&oak_repo, &history, &dest, &git_branch, force)?;

    if fresh {
        // Fill the new repository's index and working tree from the branch
        // HEAD already names (`--initial-branch`).
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(&dest)
                .args(["reset", "--hard", "--quiet"]),
            "git reset --hard",
        )?;
    }

    Ok(ExportOutcome {
        commits: history.order.len(),
        git_branch,
        merges: history.merges,
        cut_parents: history.cut_parents,
        checked_out: fresh,
    })
}

/// Refuse a branch name git can't hold, before anything is written.
fn check_git_branch_name(name: &str) -> Result<()> {
    let ok = Command::new("git")
        .args(["check-ref-format", "--branch", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(git_spawn_error)?
        .success();
    if ok {
        Ok(())
    } else {
        Err(OakError::InvalidArgument(format!(
            "'{name}' is not a valid git branch name; pass --git-branch <NAME>"
        )))
    }
}

fn git_spawn_error(e: std::io::Error) -> OakError {
    OakError::Git(format!(
        "failed to run git (is git installed and on PATH?): {e}"
    ))
}

fn run_git(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().map_err(git_spawn_error)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(OakError::Git(format!(
            "`{what}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// The commits to export, parents before children.
struct History {
    /// Oldest first; every commit's exported parents come before it.
    order: Vec<Commit>,
    /// Commits kept as merges (two exported parents).
    merges: usize,
    /// Commits whose first parent isn't stored locally.
    cut_parents: usize,
}

impl History {
    /// The parents a commit is exported with: its first parent, and its merge
    /// parent when that is on the same branch. Only parents present in the
    /// export count.
    fn parents<'a>(&self, commit: &'a Commit, kept: &HashSet<Hash>) -> Vec<&'a Hash> {
        let mut parents = Vec::with_capacity(2);
        if let Some(p) = commit.parent_hash.as_ref().filter(|p| kept.contains(*p)) {
            parents.push(p);
        }
        if let Some(m) = commit
            .merge_parent_hash
            .as_ref()
            .filter(|m| kept.contains(*m))
        {
            parents.push(m);
        }
        parents
    }
}

/// Walk back from `head`: the first-parent line, plus merge parents on the
/// same branch (and their ancestry), skipping anything not stored locally.
fn collect_history(repo: &SqliteRepository, head: &Hash) -> Result<History> {
    let mut commits: HashMap<Hash, Commit> = HashMap::new();
    let mut cut_parents = 0;
    let mut stack = vec![head.clone()];
    while let Some(hash) = stack.pop() {
        if commits.contains_key(&hash) {
            continue;
        }
        let commit = repo.get_commit(&hash)?.ok_or_else(|| {
            OakError::Server(format!("commit {} isn't stored locally", hash.short()))
        })?;
        if let Some(parent) = &commit.parent_hash {
            if repo.get_commit(parent)?.is_some() {
                stack.push(parent.clone());
            } else {
                cut_parents += 1;
            }
        }
        if let Some(merge) = &commit.merge_parent_hash {
            if let Some(m) = repo.get_commit(merge)? {
                if m.branch_name == commit.branch_name {
                    stack.push(merge.clone());
                }
            }
        }
        commits.insert(hash, commit);
    }

    // Which merge parents made it in: a merge parent is kept only when the
    // walk above followed it.
    let mut merge_kept: HashSet<Hash> = HashSet::new();
    for commit in commits.values() {
        if let Some(m) = &commit.merge_parent_hash {
            if commits
                .get(m)
                .is_some_and(|mc| mc.branch_name == commit.branch_name)
            {
                merge_kept.insert(commit.hash.clone());
            }
        }
    }

    // Parents-before-children: iterative post-order DFS from head, first
    // parent before merge parent, so a linear history comes out in order.
    let mut order: Vec<Commit> = Vec::with_capacity(commits.len());
    let mut done: HashSet<Hash> = HashSet::new();
    let mut stack: Vec<(Hash, bool)> = vec![(head.clone(), false)];
    while let Some((hash, expanded)) = stack.pop() {
        if done.contains(&hash) {
            continue;
        }
        let commit = &commits[&hash];
        if expanded {
            done.insert(hash.clone());
            order.push(commit.clone());
            continue;
        }
        stack.push((hash.clone(), true));
        if merge_kept.contains(&hash) {
            let m = commit.merge_parent_hash.clone().expect("kept merge parent");
            if !done.contains(&m) {
                stack.push((m, false));
            }
        }
        if let Some(p) = commit
            .parent_hash
            .clone()
            .filter(|p| commits.contains_key(p))
        {
            if !done.contains(&p) {
                stack.push((p, false));
            }
        }
    }

    Ok(History {
        merges: merge_kept.len(),
        order,
        cut_parents,
    })
}

type FileMap = HashMap<String, (Hash, FileMode)>;

/// Write `history` to `git fast-import` in `dest` as `refs/heads/<branch>`.
fn stream_to_fast_import(
    repo: &SqliteRepository,
    history: &History,
    dest: &Path,
    branch: &str,
    force: bool,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dest)
        .args(["fast-import", "--quiet", "--done"]);
    if force {
        cmd.arg("--force");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(git_spawn_error)?;
    let stdin = child.stdin.take().expect("piped stdin");

    let written = write_stream(repo, history, branch, BufWriter::new(stdin));
    let out = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match written {
        // A broken pipe means fast-import stopped reading; its stderr says why.
        Err(OakError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(OakError::Git(
            format!("`git fast-import` stopped: {stderr}"),
        )),
        Err(e) => Err(e),
        Ok(()) if !out.status.success() => {
            Err(OakError::Git(format!("`git fast-import` failed: {stderr}")))
        }
        Ok(()) => Ok(()),
    }
}

fn write_stream(
    repo: &SqliteRepository,
    history: &History,
    branch: &str,
    mut w: impl Write,
) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let kept: HashSet<Hash> = history.order.iter().map(|c| c.hash.clone()).collect();

    // How many exported children diff against each commit (as first parent),
    // so a commit's file map is dropped once the last one is written.
    let mut pending_children: HashMap<Hash, usize> = HashMap::new();
    for commit in &history.order {
        if let Some(first) = history.parents(commit, &kept).first() {
            *pending_children.entry((*first).clone()).or_default() += 1;
        }
    }

    let pb = ProgressBar::new(history.order.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  Exporting [{bar:30.cyan/dim}] {pos}/{len} commits")
            .unwrap()
            .progress_chars("━╸─"),
    );

    let mut next_mark: u64 = 1;
    let mut blob_marks: HashMap<Hash, u64> = HashMap::new();
    let mut commit_marks: HashMap<Hash, u64> = HashMap::new();
    let mut files: HashMap<Hash, Arc<FileMap>> = HashMap::new();
    let empty: Arc<FileMap> = Arc::new(HashMap::new());

    for commit in &history.order {
        let manifest = repo.get_manifest(&commit.manifest_hash)?.ok_or_else(|| {
            OakError::Server(format!(
                "the tree of commit {} isn't stored locally",
                commit.hash.short()
            ))
        })?;
        reject_git_internal_paths(&manifest)?;
        let current: FileMap = manifest
            .entries
            .iter()
            .map(|e| (e.path.clone(), (e.blob_hash.clone(), e.mode)))
            .collect();

        let parents = history.parents(commit, &kept);
        let base = match parents.first() {
            Some(first) => files.get(*first).cloned().unwrap_or_else(|| empty.clone()),
            None => empty.clone(),
        };

        // Blobs this commit adds or changes, written before the commit that
        // names them.
        let mut changed: Vec<&String> = current
            .iter()
            .filter(|(path, entry)| base.get(*path) != Some(entry))
            .map(|(path, _)| path)
            .collect();
        changed.sort();
        for path in &changed {
            let (hash, _) = &current[*path];
            if blob_marks.contains_key(hash) {
                continue;
            }
            let blob = repo.get_blob(hash)?.ok_or_else(|| {
                OakError::BlobNotFound(format!(
                    "the content of '{path}' (commit {}) isn't stored locally, so it can't be exported. Widen a sparse checkout with `oak sparse add`, or export from a full clone.",
                    commit.hash.short()
                ))
            })?;
            writeln!(w, "blob\nmark :{next_mark}\ndata {}", blob.content.len())?;
            w.write_all(&blob.content)?;
            w.write_all(b"\n")?;
            blob_marks.insert(hash.clone(), next_mark);
            next_mark += 1;
        }

        // A commit with no exported parent starts from nothing: `reset`
        // clears the ref, or fast-import would parent it on the ref's tip.
        if parents.is_empty() {
            writeln!(w, "reset {refname}")?;
        }
        let mark = next_mark;
        next_mark += 1;
        let (name, email) = parse_author(&commit.author);
        let when = format!("{} +0000", commit.timestamp.timestamp());
        let mut message = commit
            .message
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| format!("oak: {}", commit.hash.short()));
        if !message.ends_with('\n') {
            message.push('\n');
        }
        writeln!(w, "commit {refname}\nmark :{mark}")?;
        writeln!(w, "author {name} <{email}> {when}")?;
        writeln!(w, "committer {name} <{email}> {when}")?;
        writeln!(w, "data {}", message.len())?;
        w.write_all(message.as_bytes())?;
        if let Some(first) = parents.first() {
            writeln!(w, "from :{}", commit_marks[*first])?;
        }
        if let Some(merge) = parents.get(1) {
            writeln!(w, "merge :{}", commit_marks[*merge])?;
        }
        let mut removed: Vec<&String> = base.keys().filter(|p| !current.contains_key(*p)).collect();
        removed.sort();
        for path in removed {
            writeln!(w, "D {}", quote_path(path))?;
        }
        for path in &changed {
            let (hash, mode) = &current[*path];
            writeln!(
                w,
                "M {} :{} {}",
                git_mode(*mode),
                blob_marks[hash],
                quote_path(path)
            )?;
        }
        writeln!(w)?;

        commit_marks.insert(commit.hash.clone(), mark);
        if let Some(first) = parents.first() {
            if let Some(n) = pending_children.get_mut(*first) {
                *n -= 1;
                if *n == 0 {
                    files.remove(*first);
                }
            }
        }
        if pending_children.get(&commit.hash).is_some_and(|n| *n > 0) {
            files.insert(commit.hash.clone(), Arc::new(current));
        }
        pb.inc(1);
    }
    writeln!(w, "done")?;
    w.flush()?;
    pb.finish_and_clear();
    Ok(())
}

fn git_mode(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
    }
}

/// A path as fast-import reads it. Oak paths never hold a newline or other
/// control character, so only a leading `"` needs C-style quoting (it would
/// otherwise start a quoted path).
fn quote_path(path: &str) -> String {
    if path.starts_with('"') {
        let mut quoted = String::with_capacity(path.len() + 4);
        quoted.push('"');
        for c in path.chars() {
            if c == '"' || c == '\\' {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    } else {
        path.to_string()
    }
}

fn reject_git_internal_paths(manifest: &oak_core::Manifest) -> Result<()> {
    if let Some(entry) = manifest
        .entries
        .iter()
        .find(|entry| is_git_internal_path(&entry.path))
    {
        return Err(OakError::InvalidPath(format!(
            "cannot export path '{}': git can't hold a path inside a .git directory",
            entry.path
        )));
    }
    Ok(())
}

fn is_git_internal_path(path: &str) -> bool {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(part) => components.push(part),
            Component::Prefix(_) | Component::RootDir => return true,
        }
    }
    // Any `.git` component, in any case: `.GIT/config` is `.git/config` on
    // default macOS and Windows volumes, and a nested one is an embedded repo.
    components.iter().any(|part| {
        part.to_str()
            .is_some_and(|part| oak_core::is_metadata_dir_component(part, ".git"))
    })
}

/// Split oak's freeform `author` string into (name, email) git accepts.
/// Accepts:
///   - "Name <email>"        → (Name, email)
///   - "<email>"             → (email, email)
///   - "Name"                → (Name, name-slug@oak.local)
///
/// Git needs an email in every commit, so one is made up when the author has
/// none. Neither part may hold `<`, `>` or a line break.
fn parse_author(s: &str) -> (String, String) {
    let clean = |v: &str| -> String {
        v.chars()
            .filter(|c| !matches!(c, '<' | '>') && !c.is_control())
            .collect::<String>()
            .trim()
            .to_string()
    };
    let trimmed = s.trim();
    if let Some(open) = trimmed.rfind('<') {
        if let Some(rel_close) = trimmed[open..].find('>') {
            let email = clean(&trimmed[open + 1..open + rel_close]);
            let name = clean(&trimmed[..open]);
            if !email.is_empty() {
                let name = if name.is_empty() { email.clone() } else { name };
                return (name, email);
            }
        }
    }
    let name = clean(trimmed);
    if name.is_empty() {
        return ("Oak User".to_string(), "user@oak.local".to_string());
    }
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '.'
            }
        })
        .collect();
    let slug = collapse_dots(&slug);
    let slug = if slug.is_empty() {
        "user".to_string()
    } else {
        slug
    };
    (name, format!("{slug}@oak.local"))
}

fn collapse_dots(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dot = false;
    for c in s.chars() {
        if c == '.' {
            if !last_was_dot {
                out.push('.');
            }
            last_was_dot = true;
        } else {
            out.push(c);
            last_was_dot = false;
        }
    }
    out.trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{hash_string, Manifest, ManifestEntry};

    #[test]
    fn parse_author_name_and_email() {
        let (n, e) = parse_author("Ada Lovelace <ada@example.com>");
        assert_eq!(n, "Ada Lovelace");
        assert_eq!(e, "ada@example.com");
    }

    #[test]
    fn parse_author_just_email() {
        let (n, e) = parse_author("<ada@example.com>");
        assert_eq!(n, "ada@example.com");
        assert_eq!(e, "ada@example.com");
    }

    #[test]
    fn parse_author_just_name() {
        let (n, e) = parse_author("Ada Lovelace");
        assert_eq!(n, "Ada Lovelace");
        assert_eq!(e, "ada.lovelace@oak.local");
    }

    #[test]
    fn parse_author_blank() {
        let (n, e) = parse_author("   ");
        assert_eq!(n, "Oak User");
        assert_eq!(e, "user@oak.local");
    }

    #[test]
    fn parse_author_punctuation_collapses() {
        let (_, e) = parse_author("First   Middle    Last");
        assert_eq!(e, "first.middle.last@oak.local");
    }

    // "Name <>" falls through to the made-up email, and the empty angle
    // brackets don't reach git (which refuses `<` and `>` in a name).
    #[test]
    fn parse_author_empty_angles() {
        let (n, e) = parse_author("Ada <>");
        assert_eq!(n, "Ada");
        assert_eq!(e, "ada@oak.local");
    }

    #[test]
    fn parse_author_strips_what_git_refuses() {
        let (n, e) = parse_author("A <b> c <d@example.com>");
        assert_eq!(n, "A b c");
        assert_eq!(e, "d@example.com");
        let (n, e) = parse_author("<<x@example.com>>");
        assert!(!n.contains(['<', '>']), "{n}");
        assert!(!e.contains(['<', '>']), "{e}");
        let (n, e) = parse_author("世界");
        assert_eq!(n, "世界");
        assert_eq!(e, "user@oak.local");
    }

    #[test]
    fn quote_path_only_quotes_a_leading_quote() {
        assert_eq!(quote_path("a b/c.txt"), "a b/c.txt");
        assert_eq!(quote_path("x\"y"), "x\"y");
        assert_eq!(quote_path("back\\slash"), "back\\slash");
        assert_eq!(quote_path("\"q\\"), "\"\\\"q\\\\\"");
    }

    fn manifest_with_path(path: &str) -> Manifest {
        Manifest::new(vec![ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }])
    }

    #[test]
    fn export_rejects_paths_inside_destination_git_dir() {
        for path in [
            ".git",
            ".git/config",
            ".git/hooks/pre-commit",
            "./.git/config",
            "src/../.git/config",
            ".GIT/config",
            ".Git/hooks/pre-commit",
            "vendor/.git/config",
        ] {
            let err = reject_git_internal_paths(&manifest_with_path(path)).unwrap_err();
            assert!(
                err.to_string().contains("inside a .git directory"),
                "expected .git export guard for {path:?}, got {err}"
            );
        }
    }

    #[test]
    fn export_allows_git_named_siblings() {
        for path in [".github/workflows/ci.yml", ".gitignore", "src/.gitkeep"] {
            reject_git_internal_paths(&manifest_with_path(path)).unwrap();
        }
    }
}
