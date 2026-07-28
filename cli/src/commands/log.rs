use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::path::Path;

use oak_core::Repository;
use oak_core::{Commit, DiffLine, FileChange, FileDiff, Hash, Result};
use regex::Regex;

use crate::output;
use crate::resolve::RepoContext;

// Re-use switch logic
use super::switch;

/// Default number of commits shown when stdout is a pipe and no `-n` was
/// given. Piped output is read by agents and scripts; an unbounded history
/// dump wastes their context, and the trailing "more" hint says how to widen
/// the window when 20 isn't enough.
pub(crate) const DEFAULT_PIPED_LIMIT: usize = 20;

/// Show the commit history for the current branch
pub fn run(
    path: &Path,
    limit: Option<usize>,
    verbose: bool,
    oneline: bool,
    paths: &[String],
    search: Option<&str>,
    search_regex: Option<&str>,
) -> Result<()> {
    let work_path = path.to_path_buf();
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    // Compile the -G pattern once for the whole walk (and fail fast on a bad
    // pattern, before any history is loaded).
    let regex = search_regex.map(compile_search_regex).transpose()?;
    let regex = regex.as_ref();

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    let piped = !io::stdout().is_terminal();

    let path_filters = normalized_path_filters(&work_path, &ctx.work_tree, paths)?;
    let searching = search.is_some() || regex.is_some();
    let filtering = !path_filters.is_empty() || searching;

    // Effective limit: explicit `-n` always wins; piped output defaults to
    // a window; `--oneline` uses the same compact-script window; the
    // interactive TUI browses everything.
    let effective_limit = match limit {
        Some(n) => Some(n),
        None if piped || oneline => Some(DEFAULT_PIPED_LIMIT),
        None => None,
    };

    // Cap the walk so "is there more?" is answerable without walking (or
    // loading) the entire history: one commit past an *implicit*
    // piped/oneline window (the extra commit feeds the "more" hint), exactly
    // the window for an explicit `-n`. The cap counts *matches* — filters
    // and pickaxes are evaluated per commit as the walk streams newest to
    // oldest, so `-n 1 -G pat` stops diffing at the first match instead of
    // paying to diff the entire history first (limit-after-filter semantics,
    // as in git, are unchanged — only the evaluation order is).
    let implicit_window = limit.is_none() && (piped || oneline);
    let cap = match (effective_limit, implicit_window) {
        (Some(n), true) => Some(n.saturating_add(1)),
        (Some(n), false) => Some(n),
        (None, _) => None,
    };

    let mut pickaxe =
        searching.then(|| Pickaxe::new(repo.as_ref(), search, regex, &path_filters, false));
    let mut keep = |commit: &Commit| -> Result<bool> {
        if !path_filters.is_empty()
            && !commit
                .files
                .iter()
                .any(|change| change_matches_paths(change, &path_filters))
        {
            return Ok(false);
        }
        match pickaxe.as_mut() {
            Some(pickaxe) => Ok(pickaxe.evaluate(commit)?.is_some()),
            None => Ok(true),
        }
    };

    // Walk the commit graph from the branch's effective head via parent_hash.
    // Filtering by `commits.branch_name` would miss reachable history on a
    // personal branch created off another branch's tip, since the older
    // commits carry their original branch_name. This walk matches what
    // `oak status` and `oak commit` see at HEAD.
    let commits = match &branch_name {
        Some(name) => {
            let walk = walk_history_from_branch_filtered(repo.as_ref(), name, cap, &mut keep)?;
            warn_if_history_missing(&walk);
            walk.commits
        }
        None => {
            let mut all = repo.get_all_commits()?;
            all.reverse();
            let mut kept = Vec::new();
            for commit in all {
                if cap.is_some_and(|cap| kept.len() >= cap) {
                    break;
                }
                if keep(&commit)? {
                    kept.push(commit);
                }
            }
            kept
        }
    };

    if let Some(caveat) = pickaxe.as_ref().and_then(Pickaxe::hydration_caveat) {
        output::warning(&caveat);
    }

    if commits.is_empty() {
        if filtering {
            output::info("No matching commits");
        } else {
            output::info("No commits yet");
        }
        return Ok(());
    }

    // If stdout is not a TTY, fall back to plain-text output: compact
    // one-line-per-commit by default, the detailed block with `-v`.
    // `--oneline` requests the same compact rows even on a TTY.
    if piped || oneline {
        let shown = effective_limit.unwrap_or(commits.len());
        // The "more" hint exists to teach readers that the *implicit* piped
        // window truncated their history and how to widen it. An explicit
        // `-n` is the reader choosing the window — echoing a hint back at
        // them (as git also doesn't) only costs context.
        let truncated = limit.is_none() && (piped || oneline) && commits.len() > shown;
        // The branch being logged is implicit context for every row, so the
        // formatter omits it on rows that match and parenthesizes the rest
        // (e.g. parent-branch history reachable from this branch's tip).
        let current = branch_name.as_deref();
        if verbose {
            for commit in commits.iter().take(shown) {
                output::print_line(&output::format_commit_compact_verbose(commit, current));
            }
        } else {
            for commit in commits.iter().take(shown) {
                output::print_line(&output::format_commit_compact(commit, current));
            }
        }
        if truncated {
            // The hint must reproduce the reader's *exact* query — the
            // original cwd-relative `paths` arguments and any `-S`/`-G`
            // filter — with a doubled window. A hint that dropped the
            // filters would send whoever follows it to unrelated commits.
            output::print_line(&output::format_log_more_hint_with_mode(
                shown,
                oneline,
                paths,
                search,
                search_regex,
            ));
        }
        return Ok(());
    }

    // TTY: apply an explicit `-n` to the TUI's list as before.
    let commits: Vec<_> = if let Some(n) = effective_limit {
        commits.into_iter().take(n).collect()
    } else {
        commits
    };

    // Launch TUI
    let action = run_tui(commits, branch_name, ctx.clone())?;

    // Handle post-TUI actions
    match action {
        TuiAction::Checkout(hash) => {
            switch::run(&work_path, Some(&hash), true)?;
        }
        TuiAction::CheckoutHead => {
            let ctx = crate::resolve::resolve(&work_path)?;
            let repo = ctx.open()?;
            if let Some(branch) = repo.get_current_branch_name()?.filter(|s| !s.is_empty()) {
                switch::run(&work_path, Some(&branch), false)?;
            } else {
                output::error("No branch to return to");
            }
        }
        TuiAction::None => {}
    }

    Ok(())
}

/// Show commit history as one JSON document on stdout.
pub fn run_json(
    path: &Path,
    limit: Option<usize>,
    paths: &[String],
    search: Option<&str>,
    search_regex: Option<&str>,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch_name = repo.get_current_branch_name().ok().flatten();

    // Compile the -G pattern once for the whole walk (and fail fast on a bad
    // pattern, before any history is loaded).
    let regex = search_regex.map(compile_search_regex).transpose()?;
    let regex = regex.as_ref();

    let path_filters = normalized_path_filters(path, &ctx.work_tree, paths)?;
    let searching = search.is_some() || regex.is_some();

    // `-n` caps *matches*, evaluated as the walk streams newest to oldest —
    // see `run`. With a pickaxe active the evaluator also records which
    // paths matched per commit, so the JSON rows can carry them without the
    // reader paying for a follow-up diff per commit.
    let mut pickaxe =
        searching.then(|| Pickaxe::new(repo.as_ref(), search, regex, &path_filters, true));
    let mut matched_paths: HashMap<String, Vec<String>> = HashMap::new();
    let mut keep = |commit: &Commit| -> Result<bool> {
        if !path_filters.is_empty()
            && !commit
                .files
                .iter()
                .any(|change| change_matches_paths(change, &path_filters))
        {
            return Ok(false);
        }
        match pickaxe.as_mut() {
            Some(pickaxe) => match pickaxe.evaluate(commit)? {
                Some(paths) => {
                    matched_paths.insert(commit.hash.to_string(), paths);
                    Ok(true)
                }
                None => Ok(false),
            },
            None => Ok(true),
        }
    };

    let commits = match &branch_name {
        Some(name) => {
            let walk = walk_history_from_branch_filtered(repo.as_ref(), name, limit, &mut keep)?;
            warn_if_history_missing(&walk);
            walk.commits
        }
        None => {
            let mut all = repo.get_all_commits()?;
            all.reverse();
            let mut kept = Vec::new();
            for commit in all {
                if limit.is_some_and(|cap| kept.len() >= cap) {
                    break;
                }
                if keep(&commit)? {
                    kept.push(commit);
                }
            }
            kept
        }
    };

    // Diagnostics stay off the data channel: the document on stdout keeps
    // its documented shape (a plain array of commit objects, so a top-level
    // caveat field is not schema-compatible), and the honesty caveat goes to
    // stderr like every other warning.
    if let Some(caveat) = pickaxe.as_ref().and_then(Pickaxe::hydration_caveat) {
        output::warning(&caveat);
    }

    let json: Vec<output::LogCommitJson> = commits
        .iter()
        .map(|commit| output::LogCommitJson {
            hash: commit.hash.to_string(),
            timestamp: commit.timestamp.to_rfc3339(),
            branch: commit.branch_name.clone(),
            description_or_subject: output::commit_description_or_subject(commit),
            files_changed: commit.files.len(),
            pickaxe_matched_paths: matched_paths
                .remove(&commit.hash.to_string())
                .unwrap_or_default(),
        })
        .collect();

    output::print_json(&json)
}

/// Walk the commit graph backward from a branch's effective head, returning
/// commits in newest-first order. The effective head falls back through
/// `parent_branch` when the branch itself has no head yet, mirroring
/// `commit::resolve_effective_head`. Cycle-guarded via a seen set.
///
/// `cap` bounds how many commits are collected — callers that only display
/// `n` commits pass `n + 1` so they learn whether more history exists without
/// paying for a full walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryWalkStop {
    Complete,
    CapReached,
    MissingCommit(Hash),
}

pub(crate) struct HistoryWalk {
    pub(crate) commits: Vec<Commit>,
    pub(crate) stop: HistoryWalkStop,
}

pub(crate) fn walk_history_from_branch(
    repo: &dyn Repository,
    branch_name: &str,
    cap: Option<usize>,
) -> Result<HistoryWalk> {
    walk_history_from_branch_filtered(repo, branch_name, cap, |_| Ok(true))
}

/// [`walk_history_from_branch`] with a filter: only commits `keep` accepts
/// are collected, and `cap` bounds the *matches*. Because `keep` runs while
/// the walk streams newest→oldest, an expensive predicate (the `-S`/`-G`
/// pickaxe loads and diffs blobs) is evaluated only until the window fills,
/// never over the history behind it.
fn walk_history_from_branch_filtered(
    repo: &dyn Repository,
    branch_name: &str,
    cap: Option<usize>,
    keep: impl FnMut(&Commit) -> Result<bool>,
) -> Result<HistoryWalk> {
    let head = resolve_effective_head(repo, branch_name)?;
    walk_ancestry_filtered(repo, head, cap, keep)
}

/// Walk `head`'s parent chain newest-first, collecting commits `keep`
/// accepts, until `cap` matches are found or the chain ends. Cycle-guarded
/// via a seen set. The early exit is a contract, not an optimization:
/// callers pass predicates that read blobs, so "stop at the cap" is what
/// keeps `oak log -n 1 -G pat` from diffing an entire history.
fn walk_ancestry_filtered(
    repo: &dyn Repository,
    head: Option<Hash>,
    cap: Option<usize>,
    mut keep: impl FnMut(&Commit) -> Result<bool>,
) -> Result<HistoryWalk> {
    let mut commits = Vec::new();
    let mut seen: HashSet<Hash> = HashSet::new();
    let mut cur = head;
    let mut stop = HistoryWalkStop::Complete;
    while let Some(hash) = cur {
        if cap.is_some_and(|cap| commits.len() >= cap) {
            stop = HistoryWalkStop::CapReached;
            break;
        }
        if !seen.insert(hash.clone()) {
            break;
        }
        match repo.get_commit(&hash)? {
            Some(commit) => {
                cur = commit.parent_hash.clone();
                if keep(&commit)? {
                    commits.push(commit);
                }
            }
            None => {
                stop = HistoryWalkStop::MissingCommit(hash);
                break;
            }
        }
    }
    Ok(HistoryWalk { commits, stop })
}

pub(crate) fn warn_if_history_missing(walk: &HistoryWalk) {
    let HistoryWalkStop::MissingCommit(hash) = &walk.stop else {
        return;
    };
    // Commit in parent chain is missing from local storage — stop the walk
    // rather than erroring. This can happen when the branch head was pinned to
    // a server commit that was never downloaded (e.g. after a clone of an empty
    // repo that later received pushes before the first `oak pull`). Run
    // `oak pull` to bring local storage up to date.
    output::warning(&format!(
        "History truncated: commit {} not in local storage (run 'oak pull' to sync)",
        hash.short()
    ));
}

/// Normalize CLI path filters to repo-root-relative paths so arguments remain
/// cwd-relative when `oak log` is invoked from a subdirectory.
fn normalized_path_filters(cwd: &Path, work_tree: &Path, paths: &[String]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| crate::pathutil::repo_relative_str(cwd, work_tree, p))
        .map(|r| r.map(|p| p.trim_end_matches('/').to_string()))
        .filter(|r| !matches!(r, Ok(p) if p.is_empty()))
        .collect()
}

/// Does this change touch any of the path filters? A filter matches the path
/// exactly or as a directory prefix; renames match on either side.
fn change_matches_paths(change: &FileChange, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let hit = |path: &str| {
        filters.iter().any(|f| {
            path == f
                || (path.starts_with(f.as_str()) && path.as_bytes().get(f.len()) == Some(&b'/'))
        })
    };
    hit(&change.path) || change.old_path.as_deref().is_some_and(hit)
}

/// Compile a `-G` pattern, mapping a bad pattern to a usage error (exit
/// code 2) that carries the regex crate's own diagnostic.
pub(crate) fn compile_search_regex(pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|e| {
        oak_core::OakError::InvalidArgument(format!("invalid regex for -G/--search-regex: {e}"))
    })
}

/// Streaming pickaxe evaluator for `-S` (term-count delta) and `-G` (regex
/// over added/removed diff lines) — `git log -S` / `git log -G` semantics,
/// composed with `oak log <path>` filters. `search` and `regex` are mutually
/// exclusive (enforced at the CLI boundary).
///
/// Blob hashes come from the commit's and parent's manifests — the same
/// source `oak log`'s TUI diff uses — so the pickaxe works even when the
/// recorded `FileChange` rows carry no blob hashes. Holding the evaluator
/// across a newest→oldest walk makes it cheap per step: commit N's parent
/// manifest *is* the next-older commit's own manifest, so each manifest is
/// loaded and indexed once (hash-keyed cache) instead of twice, and lookups
/// are map hits instead of per-file linear scans.
///
/// Binary and oversized blobs are skipped by *both* pickaxes with the same
/// gate every diff-rendering path uses ([`oak_core::is_binary`] via
/// [`oak_core::binary_or_large_notice`], bounded by
/// `oak_core::MAX_TEXT_DIFF_BYTES`) — matching against them is useless, and
/// running the line differ over them is pathologically slow. This matches
/// `git log -S/-G`, which also ignores binary files.
struct Pickaxe<'a> {
    repo: &'a dyn Repository,
    search: Option<&'a str>,
    regex: Option<&'a Regex>,
    filters: &'a [String],
    /// Collect *every* matched path in a commit (for `--json` rows) instead
    /// of short-circuiting at the first matching file.
    collect_paths: bool,
    /// Files skipped because a recorded blob is not hydrated locally. The
    /// diff of such a file is unknowable, so skipping is the only honest
    /// move — but silently skipping would turn lazy clones into silent
    /// false negatives, so the count feeds [`Pickaxe::hydration_caveat`].
    skipped_unhydrated: usize,
    /// The most recently loaded parent-manifest index, keyed by its
    /// manifest hash; becomes the *commit* index of the next-older commit
    /// in a newest→oldest walk.
    cached_index: Option<(Hash, HashMap<String, Hash>)>,
}

impl<'a> Pickaxe<'a> {
    fn new(
        repo: &'a dyn Repository,
        search: Option<&'a str>,
        regex: Option<&'a Regex>,
        filters: &'a [String],
        collect_paths: bool,
    ) -> Self {
        Pickaxe {
            repo,
            search,
            regex,
            filters,
            collect_paths,
            skipped_unhydrated: 0,
            cached_index: None,
        }
    }

    /// Does the diff between `commit` and its parent match the pickaxe?
    /// Returns the matched (path-filtered) paths, or `None` for no match.
    /// Unless `collect_paths` is set, the first matching file
    /// short-circuits and the vec holds just that path.
    fn evaluate(&mut self, commit: &Commit) -> Result<Option<Vec<String>>> {
        let index = self.commit_index(commit)?;
        let (parent_manifest_hash, parent_index) = match &commit.parent_hash {
            Some(parent_hash) => match self.repo.get_commit(parent_hash)? {
                Some(parent) => {
                    let parent_index = self.load_index(&parent.manifest_hash)?;
                    (Some(parent.manifest_hash), parent_index)
                }
                None => (None, HashMap::new()),
            },
            None => (None, HashMap::new()),
        };

        let mut matched = Vec::new();
        for change in &commit.files {
            if !change_matches_paths(change, self.filters) {
                continue;
            }
            let new_bytes = blob_bytes(self.repo, &index, &change.path)?;
            let old_path = change.old_path.as_deref().unwrap_or(&change.path);
            let old_bytes = blob_bytes(self.repo, &parent_index, old_path)?;
            // Either blob unhydrated → the diff is unknowable; skip the
            // file (counted, so the caller can say so) rather than diff
            // real content against phantom emptiness.
            let (Some(old_bytes), Some(new_bytes)) = (old_bytes, new_bytes) else {
                self.skipped_unhydrated += 1;
                continue;
            };
            if old_bytes == new_bytes {
                continue;
            }
            if oak_core::binary_or_large_notice(&change.path, &old_bytes, &new_bytes).is_some() {
                continue;
            }
            if self.file_matches(&change.path, &old_bytes, &new_bytes) {
                matched.push(change.path.clone());
                if !self.collect_paths {
                    break;
                }
            }
        }

        // Hand the parent's index to the next (older) step of the walk.
        self.cached_index = parent_manifest_hash.map(|hash| (hash, parent_index));

        Ok(if matched.is_empty() {
            None
        } else {
            Some(matched)
        })
    }

    /// One warning-worthy sentence when unhydrated blobs were skipped, so a
    /// lazy clone's incomplete answer is never presented as a complete one.
    fn hydration_caveat(&self) -> Option<String> {
        (self.skipped_unhydrated > 0).then(|| {
            format!(
                "{} file(s) skipped: content not hydrated locally; \
                 pickaxe results may be incomplete (run 'oak pull' to hydrate)",
                self.skipped_unhydrated
            )
        })
    }

    /// Does one changed file match? `-S`: the term's occurrence count
    /// differs between the two sides. `-G`: an added or removed diff line
    /// matches the regex — unlike `-S`, a term that merely *moves* (same
    /// total count) still matches, because the move shows up as removed +
    /// added lines.
    fn file_matches(&self, path: &str, old_bytes: &[u8], new_bytes: &[u8]) -> bool {
        let old_content = String::from_utf8_lossy(old_bytes);
        let new_content = String::from_utf8_lossy(new_bytes);
        if let Some(term) = self.search {
            return old_content.matches(term).count() != new_content.matches(term).count();
        }
        if let Some(regex) = self.regex {
            let diff = FileDiff::new(path, &old_content, &new_content);
            return diff.lines.iter().any(|line| match line {
                DiffLine::Added(s) | DiffLine::Removed(s) => regex.is_match(s),
                DiffLine::Context(_) => false,
            });
        }
        false
    }

    /// The commit's own manifest index: reused from the previous walk step
    /// when the hashes line up, loaded otherwise.
    fn commit_index(&mut self, commit: &Commit) -> Result<HashMap<String, Hash>> {
        if let Some((hash, index)) = self.cached_index.take() {
            if hash == commit.manifest_hash {
                return Ok(index);
            }
        }
        self.load_index(&commit.manifest_hash)
    }

    /// Load a manifest and index it by path. A missing manifest (root
    /// commits, unfetched history) indexes as empty.
    fn load_index(&self, manifest_hash: &Hash) -> Result<HashMap<String, Hash>> {
        Ok(match self.repo.get_manifest(manifest_hash)? {
            Some(manifest) => manifest
                .entries
                .into_iter()
                .map(|entry| (entry.path, entry.blob_hash))
                .collect(),
            None => HashMap::new(),
        })
    }
}

/// The content of the blob `path` points at within an indexed manifest.
///
/// Distinguishes two very different absences: a path missing from the
/// manifest is a *real* empty side (the added/deleted half of a change) and
/// yields `Some(empty)`; a path whose blob is recorded but not hydrated
/// locally yields `None` — its content is unknown, and treating it as empty
/// would make every line of the other side look changed (pickaxe false
/// positives on lazily-hydrated clones).
fn blob_bytes(
    repo: &dyn Repository,
    index: &HashMap<String, Hash>,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(blob_hash) = index.get(path) else {
        return Ok(Some(Vec::new()));
    };
    Ok(repo.get_blob(blob_hash)?.map(|b| b.content))
}

fn resolve_effective_head(repo: &dyn Repository, name: &str) -> Result<Option<Hash>> {
    let mut seen = HashSet::new();
    let mut current = name.to_string();
    loop {
        if !seen.insert(current.clone()) {
            return Ok(None);
        }
        if let Some(h) = repo.get_branch_head(&current)? {
            return Ok(Some(h));
        }
        match repo.get_branch(&current)? {
            Some(b) => match b.parent_branch {
                Some(p) => current = p,
                None => return Ok(None),
            },
            None => return Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// TUI implementation
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

/// Which pane currently owns the keyboard: the commit list on the left, or the
/// scrollable detail/diff pane on the right. Mirrors the `Focus` model in the
/// `oak diff` file-tree browser so the two TUIs feel the same.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Detail,
}

enum TuiAction {
    None,
    Checkout(String), // commit hash string
    CheckoutHead,     // return to branch HEAD
}

struct App {
    commits: Vec<Commit>,
    branch_name: Option<String>,
    ctx: RepoContext,
    /// Cursor position *within* `filtered_indices` (not into `commits`).
    selected: usize,
    filtered_indices: Vec<usize>,
    focus: Focus,
    /// Scroll offset into the rendered detail pane for the current commit.
    detail_scroll: usize,
    /// Lazily-built detail+diff line blocks, keyed by index into `commits`.
    /// Computing a diff opens the repo and reads blobs, so we build a commit's
    /// detail once on first view and reuse it as the cursor moves around.
    detail_cache: HashMap<usize, Vec<Line<'static>>>,
    search_active: bool,
    search_query: String,
    quit: bool,
    action: TuiAction,
}

impl App {
    fn new(commits: Vec<Commit>, branch_name: Option<String>, ctx: RepoContext) -> Self {
        let filtered_indices: Vec<usize> = (0..commits.len()).collect();
        App {
            commits,
            branch_name,
            ctx,
            selected: 0,
            filtered_indices,
            focus: Focus::List,
            detail_scroll: 0,
            detail_cache: HashMap::new(),
            search_active: false,
            search_query: String::new(),
            quit: false,
            action: TuiAction::None,
        }
    }

    fn update_filter(&mut self) {
        let query = self.search_query.to_lowercase();
        if query.is_empty() {
            self.filtered_indices = (0..self.commits.len()).collect();
        } else {
            self.filtered_indices = self
                .commits
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.hash.short().to_lowercase().contains(&query)
                        || c.author.to_lowercase().contains(&query)
                        || c.message
                            .as_deref()
                            .map(|m| m.to_lowercase().contains(&query))
                            .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();
        }
        self.selected = 0;
        self.detail_scroll = 0;
    }

    fn selected_commit_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let len = self.filtered_indices.len() as i32;
        let next = (self.selected as i32)
            .saturating_add(delta)
            .clamp(0, len - 1) as usize;
        if next != self.selected {
            self.selected = next;
            self.detail_scroll = 0;
        }
    }

    fn select_first(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected = 0;
            self.detail_scroll = 0;
        }
    }

    fn select_last(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected = self.filtered_indices.len() - 1;
            self.detail_scroll = 0;
        }
    }

    fn scroll_detail(&mut self, delta: i32) {
        let next = self.detail_scroll as i32 + delta;
        self.detail_scroll = next.max(0) as usize; // upper bound clamped at draw
    }

    /// Build (and memoize) the right-pane content for a commit: metadata header,
    /// message, changed-file list, then the unified diff against its parent.
    fn detail_lines(&mut self, commit_idx: usize) -> &Vec<Line<'static>> {
        if !self.detail_cache.contains_key(&commit_idx) {
            let lines = build_detail(&self.commits, &self.ctx, commit_idx);
            self.detail_cache.insert(commit_idx, lines);
        }
        &self.detail_cache[&commit_idx]
    }
}

/// Compute the right-pane content for one commit. Pulled out as a free function
/// (rather than an `&self` method) so the caller can hold `&mut self.detail_cache`
/// while building, avoiding a partial-borrow conflict.
fn build_detail(commits: &[Commit], ctx: &RepoContext, commit_idx: usize) -> Vec<Line<'static>> {
    let commit = &commits[commit_idx];
    let label = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("commit  ", label),
            Span::raw(commit.hash.to_string()),
        ]),
        Line::from(vec![
            Span::styled("branch  ", label),
            Span::raw(commit.branch_name.clone()),
        ]),
        Line::from(vec![
            Span::styled("author  ", label),
            Span::raw(commit.author.clone()),
        ]),
        Line::from(vec![
            Span::styled("date    ", label),
            Span::raw(commit.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
        ]),
        Line::from(""),
    ];

    if let Some(msg) = commit.message.as_deref().filter(|m| !m.is_empty()) {
        for l in msg.lines() {
            lines.push(Line::from(l.to_string()));
        }
    }

    if !commit.files.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Files changed:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for file in &commit.files {
            let (prefix, color) = match file.change_type {
                oak_core::ChangeType::Added => ("A", Color::Green),
                oak_core::ChangeType::Modified => ("M", Color::Yellow),
                oak_core::ChangeType::Deleted => ("D", Color::Red),
                oak_core::ChangeType::Renamed => ("R", Color::Cyan),
            };
            let display_path = match (file.change_type, &file.old_path) {
                (oak_core::ChangeType::Renamed, Some(old)) => format!("{old} -> {}", file.path),
                _ => file.path.clone(),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {prefix} "), Style::default().fg(color)),
                Span::raw(display_path),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "──── diff ────",
        Style::default().fg(Color::DarkGray),
    )));

    match compute_diff_lines(ctx, commit) {
        Ok(diff) => lines.extend(diff),
        Err(e) => lines.push(Line::from(Span::styled(
            format!("Error computing diff: {e}"),
            Style::default().fg(Color::Red),
        ))),
    }

    lines
}

/// Compute the unified diff of a commit against its parent, as styled lines.
fn compute_diff_lines(ctx: &RepoContext, commit: &Commit) -> Result<Vec<Line<'static>>> {
    let repo = ctx.open()?;

    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .unwrap_or_else(oak_core::Manifest::empty);

    let parent_manifest = if let Some(ref parent_hash) = commit.parent_hash {
        if let Some(parent_commit) = repo.get_commit(parent_hash)? {
            repo.get_manifest(&parent_commit.manifest_hash)?
                .unwrap_or_else(oak_core::Manifest::empty)
        } else {
            oak_core::Manifest::empty()
        }
    } else {
        oak_core::Manifest::empty()
    };

    let old_entries: HashMap<&str, &oak_core::ManifestEntry> = parent_manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let new_entries: HashMap<&str, &oak_core::ManifestEntry> = manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut all_paths: Vec<&str> = old_entries
        .keys()
        .chain(new_entries.keys())
        .copied()
        .collect();
    all_paths.sort();
    all_paths.dedup();

    // A changed file is rendered either as a textual unified diff or, when it's
    // binary or too large, as a one-line notice. Diffing a NUL-laden or
    // multi-megabyte blob (assets, packfiles, the binaries in an import commit)
    // is both useless and pathologically slow — running the line differ over a
    // whole tree of them is what otherwise freezes `oak log` on large repos.
    enum FileBlock {
        Text(FileDiff),
        Notice { path: String, notice: String },
    }

    let read_bytes = |hash: Option<&Hash>| -> Result<Vec<u8>> {
        match hash {
            Some(hash) => Ok(repo.get_blob(hash)?.map(|b| b.content).unwrap_or_default()),
            None => Ok(Vec::new()),
        }
    };

    let mut blocks = Vec::new();
    for path in all_paths {
        let old_hash = old_entries.get(path).map(|e| &e.blob_hash);
        let new_hash = new_entries.get(path).map(|e| &e.blob_hash);
        if old_hash == new_hash {
            continue;
        }
        let old_bytes = read_bytes(old_hash)?;
        let new_bytes = read_bytes(new_hash)?;
        if let Some(notice) = oak_core::binary_or_large_notice(path, &old_bytes, &new_bytes) {
            blocks.push(FileBlock::Notice {
                path: path.to_string(),
                notice,
            });
            continue;
        }
        let old_content = String::from_utf8_lossy(&old_bytes);
        let new_content = String::from_utf8_lossy(&new_bytes);
        let diff = FileDiff::new(path, &old_content, &new_content);
        if diff.has_changes {
            blocks.push(FileBlock::Text(diff));
        }
    }

    let mut lines = Vec::new();
    if blocks.is_empty() {
        lines.push(Line::from(Span::styled(
            "No differences to display",
            Style::default().fg(Color::DarkGray),
        )));
        return Ok(lines);
    }

    for block in &blocks {
        match block {
            FileBlock::Text(diff) => {
                for line in diff.to_unified().lines() {
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        style_diff_line(line),
                    )));
                }
            }
            FileBlock::Notice { path, notice } => {
                lines.push(Line::from(Span::styled(
                    format!("diff --oak a/{path} b/{path}"),
                    style_diff_line("diff --oak"),
                )));
                lines.push(Line::from(Span::styled(
                    notice.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        lines.push(Line::from(""));
    }
    Ok(lines)
}

/// Style a unified-diff line by its prefix — matches the `oak diff` browser.
fn style_diff_line(line: &str) -> Style {
    if line.starts_with('+') && !line.starts_with("+++") {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with("diff ") || line.starts_with("--- ") || line.starts_with("+++ ") {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// The concatenated plain text of a styled `Line`, used only for measuring how
/// many terminal rows it occupies once wrapped.
fn line_plain_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Count how many terminal rows `text` occupies when word-wrapped to `width`,
/// approximating ratatui's `Wrap { trim: false }`. Greedy by words, breaking any
/// single word longer than the width. An empty line still occupies one row.
fn wrapped_rows(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1usize;
    let mut col = 0usize;
    for word in text.split(' ') {
        let wlen = word.chars().count();
        if wlen > width {
            // Long word: flush the current row, then break the word across rows.
            if col > 0 {
                rows += 1;
            }
            let mut remaining = wlen;
            while remaining > width {
                rows += 1;
                remaining -= width;
            }
            col = remaining;
        } else {
            let needed = if col == 0 { wlen } else { wlen + 1 };
            if col + needed > width {
                rows += 1;
                col = wlen;
            } else {
                col += needed;
            }
        }
    }
    rows
}

/// RAII guard for the TUI's terminal state. `enter` switches the terminal into
/// raw mode + the alternate screen; `Drop` restores it. Because `Drop` runs on
/// the normal return path, on any `?` early-return inside the event loop, *and*
/// while unwinding from a panic, the terminal is never left wedged in raw mode
/// or stuck in the alternate screen.
///
/// The `Show` is the crucial bit: ratatui's `Terminal::draw` hides the cursor on
/// every frame that doesn't set a cursor position, and cursor visibility is a
/// terminal-global setting (DECTCEM) that survives leaving the alternate screen.
/// Without an explicit re-show, quitting the viewer leaves the user's shell
/// cursor invisible.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore in reverse order; errors are ignored because we
        // may already be tearing down (e.g. mid-panic) with nowhere to report.
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(Show);
        let _ = disable_raw_mode();
    }
}

fn run_tui(
    commits: Vec<Commit>,
    branch_name: Option<String>,
    ctx: RepoContext,
) -> Result<TuiAction> {
    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(commits, branch_name, ctx);

    while !app.quit {
        terminal.draw(|f| draw(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(&mut app, key);
            }
        }
    }

    Ok(app.action)
    // `_guard` drops here: leaves the alternate screen, re-shows the cursor,
    // and disables raw mode — on every exit path, including `?` and panics.
}

fn handle_key(app: &mut App, event: KeyEvent) {
    let key = event.code;
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);

    if app.search_active {
        match key {
            KeyCode::Esc | KeyCode::Enter => app.search_active = false,
            KeyCode::Backspace => {
                app.search_query.pop();
                app.update_filter();
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                app.update_filter();
            }
            _ => {}
        }
        return;
    }

    match app.focus {
        Focus::List => match key {
            KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
            KeyCode::Char('/') => app.search_active = true,
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::PageUp => app.move_selection(-20),
            KeyCode::PageDown => app.move_selection(20),
            KeyCode::Char('b') if ctrl => app.move_selection(-20),
            KeyCode::Char('f') if ctrl => app.move_selection(20),
            KeyCode::Char('u') if ctrl => app.move_selection(-10),
            KeyCode::Char('d') if ctrl => app.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => app.select_first(),
            KeyCode::End | KeyCode::Char('G') => app.select_last(),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                if app.selected_commit_index().is_some() {
                    app.focus = Focus::Detail;
                }
            }
            KeyCode::Char('c') => {
                if let Some(idx) = app.selected_commit_index() {
                    app.action = TuiAction::Checkout(app.commits[idx].hash.to_string());
                    app.quit = true;
                }
            }
            KeyCode::Char('r') => {
                app.action = TuiAction::CheckoutHead;
                app.quit = true;
            }
            _ => {}
        },
        Focus::Detail => match key {
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Tab => {
                app.focus = Focus::List;
            }
            KeyCode::Up | KeyCode::Char('k') => app.scroll_detail(-1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_detail(1),
            KeyCode::PageUp => app.scroll_detail(-20),
            KeyCode::PageDown => app.scroll_detail(20),
            KeyCode::Char('b') if ctrl => app.scroll_detail(-20),
            KeyCode::Char('f') if ctrl => app.scroll_detail(20),
            KeyCode::Char('u') if ctrl => app.scroll_detail(-10),
            KeyCode::Char('d') if ctrl => app.scroll_detail(10),
            KeyCode::Home | KeyCode::Char('g') => app.detail_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => app.detail_scroll = usize::MAX,
            KeyCode::Char('c') => {
                if let Some(idx) = app.selected_commit_index() {
                    app.action = TuiAction::Checkout(app.commits[idx].hash.to_string());
                    app.quit = true;
                }
            }
            KeyCode::Char('r') => {
                app.action = TuiAction::CheckoutHead;
                app.quit = true;
            }
            _ => {}
        },
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(52), Constraint::Min(30)])
        .split(outer[0]);

    draw_list(f, app, panes[0]);
    draw_detail(f, app, panes[1]);

    let help = if app.focus == Focus::List {
        " ↑↓/jk move · Enter/→/Tab details · c switch · r HEAD · / search · q quit"
    } else {
        " ↑↓/jk scroll · PgUp/PgDn/^F/^B page · ^U/^D half · g/G top/bottom · ←/Esc/Tab back · q quit"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        outer[1],
    );
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let list_focused = app.focus == Focus::List;

    // When searching, carve a 3-line search box off the top of the left pane.
    let show_search = app.search_active || !app.search_query.is_empty();
    let (list_area, search_area) = if show_search {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        (chunks[1], Some(chunks[0]))
    } else {
        (area, None)
    };

    if let Some(search_area) = search_area {
        let style = if app.search_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let search = Paragraph::new(format!("/{}", app.search_query))
            .style(style)
            .block(Block::default().borders(Borders::ALL).title(" Search "));
        f.render_widget(search, search_area);
    }

    let selected_pos = if app.filtered_indices.is_empty() {
        0
    } else {
        app.selected + 1
    };
    let total = app.filtered_indices.len();
    let title = match &app.branch_name {
        Some(name) => format!(" log · {name} · {selected_pos}/{total} "),
        None => format!(" log · {selected_pos}/{total} "),
    };

    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&i| {
            let c = &app.commits[i];
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", c.hash.short()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(c.author.clone(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let border_style = if list_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let highlight = if list_focused {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(highlight)
        .highlight_symbol("\u{25b8} ");

    let mut state = ListState::default();
    if !app.filtered_indices.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, list_area, &mut state);
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let detail_focused = app.focus == Focus::Detail;
    let border_style = if detail_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let Some(commit_idx) = app.selected_commit_index() else {
        let para = Paragraph::new(Line::from(Span::styled(
            "No commit selected",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(" detail "),
        );
        f.render_widget(para, area);
        return;
    };

    let title = {
        let c = &app.commits[commit_idx];
        match c.message.as_deref().and_then(|m| m.lines().next()) {
            Some(msg) if !msg.is_empty() => format!(" {} · {} ", c.hash.short(), msg),
            _ => format!(" {} ", c.hash.short()),
        }
    };

    // Clone the cached lines so we can mutate `app.detail_scroll` afterwards
    // without holding an immutable borrow of the cache.
    let lines = app.detail_lines(commit_idx).clone();

    let inner_width = area.width.saturating_sub(2) as usize;
    let content_height = area.height.saturating_sub(2) as usize;

    // Clamp the scroll to the wrapped height so `G` / PageDown can't scroll past
    // the end into blank space. ratatui's own `line_count` is behind an unstable
    // feature, so we count wrapped rows ourselves (close enough for clamping).
    let total: usize = lines
        .iter()
        .map(|l| wrapped_rows(&line_plain_text(l), inner_width))
        .sum();
    let max_scroll = total.saturating_sub(content_height);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let scroll = app.detail_scroll.min(u16::MAX as usize) as u16;

    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    f.render_widget(para, area);
}

#[cfg(test)]
mod pickaxe_tests {
    use super::*;
    use oak_core::{ChangeType, FileChange, FileMode, ManifestEntry, SqliteRepository};
    use tempfile::TempDir;

    fn entry(path: &str, blob_hash: Hash) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash,
            mode: FileMode::Regular,
        }
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            change_type: ChangeType::Modified,
            old_path: None,
            old_blob_hash: None,
            new_blob_hash: None,
            old_mode: None,
            new_mode: None,
        }
    }

    fn regex_matches(repo: &dyn Repository, commit: &Commit, regex: &Regex) -> bool {
        Pickaxe::new(repo, None, Some(regex), &[], false)
            .evaluate(commit)
            .unwrap()
            .is_some()
    }

    fn term_matches(repo: &dyn Repository, commit: &Commit, term: &str) -> bool {
        Pickaxe::new(repo, Some(term), None, &[], false)
            .evaluate(commit)
            .unwrap()
            .is_some()
    }

    /// A parent whose blob is recorded in the manifest but not hydrated
    /// locally must be *skipped* by both pickaxes — diffing real content
    /// against phantom emptiness would make every pattern in the file
    /// match — and the skip must be counted so the caller can warn instead
    /// of presenting an incomplete answer as a complete one. Hydrating the
    /// blob flips the same commit to a match.
    #[test]
    fn pickaxes_skip_and_count_files_whose_blobs_are_not_hydrated_locally() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let old_content = b"fn hidden_original() {}\n".to_vec();
        let old_hash = oak_core::hash_bytes(&old_content); // NOT stored
        let parent_manifest = repo.put_manifest(vec![entry("code.rs", old_hash)]).unwrap();
        let parent_hash = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                parent_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();

        let new_hash = repo.put_blob(b"fn visible_now() {}\n".to_vec()).unwrap();
        let child_manifest = repo.put_manifest(vec![entry("code.rs", new_hash)]).unwrap();
        let child_hash = repo
            .put_commit(
                "main".to_string(),
                Some(parent_hash),
                None,
                child_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![modified("code.rs")],
            )
            .unwrap();
        let commit = repo.get_commit(&child_hash).unwrap().unwrap();

        let regex = Regex::new("visible_now").unwrap();
        let mut pickaxe = Pickaxe::new(&repo, None, Some(&regex), &[], false);
        assert!(
            pickaxe.evaluate(&commit).unwrap().is_none(),
            "-G must skip a file whose old blob is unhydrated, not match everything in it"
        );
        assert_eq!(
            pickaxe.skipped_unhydrated, 1,
            "the skip must be counted, not silent"
        );
        let caveat = pickaxe.hydration_caveat().expect("caveat for skipped file");
        assert!(
            caveat.contains("1 file(s) skipped") && caveat.contains("not hydrated"),
            "caveat should say what was skipped and why: {caveat}"
        );
        assert!(
            !term_matches(&repo, &commit, "visible_now"),
            "-S must skip a file whose old blob is unhydrated"
        );

        // Hydrate the parent blob: now the comparison is knowable and the
        // commit genuinely adds the pattern.
        repo.put_blob(old_content).unwrap();
        assert!(regex_matches(&repo, &commit, &regex));
        assert!(term_matches(&repo, &commit, "visible_now"));
        let mut hydrated = Pickaxe::new(&repo, None, Some(&regex), &[], false);
        hydrated.evaluate(&commit).unwrap();
        assert_eq!(hydrated.skipped_unhydrated, 0);
        assert!(hydrated.hydration_caveat().is_none());
    }

    /// A path absent from the parent manifest is a *real* empty side (the
    /// file was added); the pickaxes must still match those.
    #[test]
    fn pickaxes_match_added_files_against_a_true_empty_side() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let parent_manifest = repo.put_manifest(Vec::new()).unwrap();
        let parent_hash = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                parent_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        let new_hash = repo.put_blob(b"fn brand_new() {}\n".to_vec()).unwrap();
        let child_manifest = repo.put_manifest(vec![entry("new.rs", new_hash)]).unwrap();
        let child_hash = repo
            .put_commit(
                "main".to_string(),
                Some(parent_hash),
                None,
                child_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![modified("new.rs")],
            )
            .unwrap();
        let commit = repo.get_commit(&child_hash).unwrap().unwrap();

        let regex = Regex::new("brand_new").unwrap();
        assert!(regex_matches(&repo, &commit, &regex));
        assert!(term_matches(&repo, &commit, "brand_new"));
    }

    /// Both pickaxes skip binary blobs with the diff renderers' shared gate:
    /// a term "added" inside a binary file matches neither `-S` nor `-G`,
    /// exactly as in git.
    #[test]
    fn pickaxes_skip_binary_blobs_with_the_shared_gate() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let parent_manifest = repo.put_manifest(Vec::new()).unwrap();
        let parent_hash = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                parent_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        // NUL bytes make the blob binary for `oak_core::is_binary`.
        let new_hash = repo
            .put_blob(b"needle\x00\x01\x02binary payload\n".to_vec())
            .unwrap();
        let child_manifest = repo
            .put_manifest(vec![entry("asset.bin", new_hash)])
            .unwrap();
        let child_hash = repo
            .put_commit(
                "main".to_string(),
                Some(parent_hash),
                None,
                child_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![modified("asset.bin")],
            )
            .unwrap();
        let commit = repo.get_commit(&child_hash).unwrap().unwrap();

        let regex = Regex::new("needle").unwrap();
        assert!(
            !regex_matches(&repo, &commit, &regex),
            "-G must not match inside binary blobs"
        );
        assert!(
            !term_matches(&repo, &commit, "needle"),
            "-S must apply the same binary gate as -G"
        );
    }

    /// With `collect_paths` the evaluator reports *every* matched path in
    /// the commit (the `--json` contract), not just the first hit.
    #[test]
    fn pickaxe_collects_all_matched_paths_when_asked() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let parent_manifest = repo.put_manifest(Vec::new()).unwrap();
        let parent_hash = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                parent_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        let a = repo.put_blob(b"alpha needle\n".to_vec()).unwrap();
        let b = repo.put_blob(b"no match here\n".to_vec()).unwrap();
        let c = repo.put_blob(b"another needle\n".to_vec()).unwrap();
        let child_manifest = repo
            .put_manifest(vec![
                entry("a.txt", a),
                entry("b.txt", b),
                entry("c.txt", c),
            ])
            .unwrap();
        let child_hash = repo
            .put_commit(
                "main".to_string(),
                Some(parent_hash),
                None,
                child_manifest,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![modified("a.txt"), modified("b.txt"), modified("c.txt")],
            )
            .unwrap();
        let commit = repo.get_commit(&child_hash).unwrap().unwrap();

        let regex = Regex::new("needle").unwrap();
        let mut collecting = Pickaxe::new(&repo, None, Some(&regex), &[], true);
        assert_eq!(
            collecting.evaluate(&commit).unwrap().unwrap(),
            vec!["a.txt".to_string(), "c.txt".to_string()],
            "--json rows list every matched path"
        );

        let mut short_circuiting = Pickaxe::new(&repo, None, Some(&regex), &[], false);
        assert_eq!(
            short_circuiting.evaluate(&commit).unwrap().unwrap(),
            vec!["a.txt".to_string()],
            "without collect_paths the first hit short-circuits"
        );
    }

    /// The filtered walk stops loading history the moment the cap is
    /// reached: the predicate — which for a pickaxe means loading and
    /// diffing blobs — must never run against commits older than the
    /// requested window's last match.
    #[test]
    fn filtered_walk_stops_evaluating_once_the_match_cap_is_reached() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let manifest = repo.put_manifest(Vec::new()).unwrap();
        let mut parent: Option<Hash> = None;
        let mut heads = Vec::new();
        for _ in 0..5 {
            let hash = repo
                .put_commit(
                    "main".to_string(),
                    parent.clone(),
                    None,
                    manifest.clone(),
                    "tester".to_string(),
                    None,
                    chrono::Utc::now(),
                    Vec::new(),
                )
                .unwrap();
            parent = Some(hash.clone());
            heads.push(hash);
        }
        let head = heads.last().cloned();

        // Every commit matches: cap 2 → exactly 2 evaluations, walk stops.
        let mut evaluated = 0;
        let walk = walk_ancestry_filtered(&repo, head.clone(), Some(2), |_| {
            evaluated += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(walk.commits.len(), 2);
        assert_eq!(
            evaluated, 2,
            "predicate must not run past the filled window"
        );
        assert_eq!(walk.stop, HistoryWalkStop::CapReached);

        // Sparse matches: the walk keeps going until the cap is *matched*,
        // then stops — here the 4th-newest commit fills a cap of 2.
        let mut evaluated = 0;
        let walk = walk_ancestry_filtered(&repo, head, Some(2), |_| {
            evaluated += 1;
            Ok(evaluated % 2 == 0)
        })
        .unwrap();
        assert_eq!(walk.commits.len(), 2);
        assert_eq!(evaluated, 4, "walk stops at the second match, not before");
        assert_eq!(walk.stop, HistoryWalkStop::CapReached);
    }
}
