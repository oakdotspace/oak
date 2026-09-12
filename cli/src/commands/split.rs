//! `oak split` — split a branch's commits into independent branches.
//!
//! This is Oak's answer to `git rebase -i` / `sl histedit`, plus a split step.
//! You take the commits a branch has accumulated on top of `main` and:
//!
//!   * **split** the sequence into several branches,
//!   * **reorder** them, and
//!   * **drop** ones you don't want.
//!
//! Oak uses a flat branch model — every branch parents onto the trunk
//! (`main`), never onto another branch — so a split produces **independent
//! branches off `main`**, not a stack. Each resulting branch is the chosen
//! commits replayed onto the branch's original base. Because the segments are
//! independent, a later segment that actually depends on an earlier one (it
//! edits a file the earlier segment introduced) will **conflict** on replay;
//! split reports the conflicting paths and changes nothing rather than
//! producing a branch that can't stand on its own.
//!
//! Two front ends share one engine:
//!
//!   * **Interactive** (`oak split`) — a TUI to reorder / drop / split.
//!   * **Scriptable** (`oak split --plan <file|->`) — a `rebase -i`-style
//!     todo list, so a coding agent can drive it without a terminal.
//!     `--dry-run` prints the resulting structure without writing anything.

use std::io::Read;
use std::path::Path;

use oak_core::{
    three_way_merge_manifests, Branch, Commit, Hash, Manifest, OakError, Repository, Result,
    DEFAULT_BRANCH,
};

use crate::output;

// ---------------------------------------------------------------------------
// Plan model
// ---------------------------------------------------------------------------

/// A parsed, validated plan: an ordered list of segments. Segment 0 is the
/// source branch (rewritten in place); each later segment becomes a new branch
/// off `main`.
struct Plan {
    segments: Vec<Segment>,
}

struct Segment {
    /// `None` for the first segment (the source branch); `Some(name)` for a
    /// split target — a fresh branch off `main`.
    branch: Option<String>,
    /// Commit hashes to replay, in order.
    picks: Vec<Hash>,
}

// ---------------------------------------------------------------------------
// Branch history
// ---------------------------------------------------------------------------

/// The slice of history `split` operates on: the branch's own commits
/// (oldest first) and the commit/manifest they fork from on `main`.
struct BranchHistory {
    /// The commit on `main` this branch forked from, if any. `None` for a root
    /// branch with no parent commit.
    base_commit: Option<Hash>,
    base_manifest: Manifest,
    /// The branch's own commits, oldest first.
    commits: Vec<Commit>,
}

/// Walk back from a branch's head, collecting the commits that belong to it
/// (`branch_name == branch`) until we reach the base commit on the trunk.
fn load_history(repo: &dyn Repository, branch: &str) -> Result<BranchHistory> {
    let head = repo.get_branch_head(branch)?.ok_or_else(|| {
        OakError::InvalidArgument(format!("branch '{branch}' has no head commit"))
    })?;

    let mut chain: Vec<Commit> = Vec::new();
    let mut base_commit: Option<Hash> = None;
    let mut cur = Some(head);
    while let Some(h) = cur {
        let commit = repo
            .get_commit(&h)?
            .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
        if commit.branch_name == branch {
            cur = commit.parent_hash.clone();
            chain.push(commit);
        } else {
            // First commit not on this branch is the base on the trunk.
            base_commit = Some(h);
            break;
        }
    }
    chain.reverse(); // oldest first

    if chain.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "branch '{branch}' has no commits of its own to edit"
        )));
    }

    let base_manifest = match &base_commit {
        Some(h) => {
            let base = repo
                .get_commit(h)?
                .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
            repo.get_manifest(&base.manifest_hash)?
                .unwrap_or_else(Manifest::empty)
        }
        None => Manifest::empty(),
    };

    Ok(BranchHistory {
        base_commit,
        base_manifest,
        commits: chain,
    })
}

// ---------------------------------------------------------------------------
// Plan parsing
// ---------------------------------------------------------------------------

/// A one-line, human-facing label for a commit (commits carry no message in
/// Oak, so fall back to author + first changed path).
fn summarize(c: &Commit) -> String {
    let first = c.files.first().map(|f| f.path.as_str()).unwrap_or("");
    let n = c.files.len();
    if n == 0 {
        format!("(empty) {}", c.author)
    } else if n == 1 {
        first.to_string()
    } else {
        format!("{first} (+{} more)", n - 1)
    }
}

/// Parse a todo-list plan against a branch's history.
fn parse_plan(text: &str, history: &BranchHistory) -> Result<Plan> {
    let resolve = |prefix: &str| -> Result<Hash> {
        let matches: Vec<&Commit> = history
            .commits
            .iter()
            .filter(|c| c.hash.to_string().starts_with(prefix))
            .collect();
        match matches.as_slice() {
            [c] => Ok(c.hash.clone()),
            [] => Err(OakError::InvalidArgument(format!(
                "no commit on this branch matches '{prefix}'"
            ))),
            _ => Err(OakError::InvalidArgument(format!(
                "commit prefix '{prefix}' is ambiguous"
            ))),
        }
    };

    let mut segments: Vec<Segment> = vec![Segment {
        branch: None,
        picks: Vec::new(),
    }];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kw = parts.next().unwrap_or("");
        let arg = parts.next();
        let lineno = i + 1;
        match kw {
            "pick" | "p" => {
                let hash = resolve(arg.ok_or_else(|| {
                    OakError::InvalidArgument(format!("line {lineno}: `pick` needs a commit"))
                })?)?;
                if !seen.insert(hash.to_string()) {
                    return Err(OakError::InvalidArgument(format!(
                        "line {lineno}: commit {} listed more than once",
                        hash.short()
                    )));
                }
                segments.last_mut().unwrap().picks.push(hash);
            }
            "drop" | "d" => {
                let hash = resolve(arg.ok_or_else(|| {
                    OakError::InvalidArgument(format!("line {lineno}: `drop` needs a commit"))
                })?)?;
                if !seen.insert(hash.to_string()) {
                    return Err(OakError::InvalidArgument(format!(
                        "line {lineno}: commit {} listed more than once",
                        hash.short()
                    )));
                }
                // Dropped: recorded as accounted-for, but not replayed.
            }
            "split" | "s" => {
                let name = arg
                    .ok_or_else(|| {
                        OakError::InvalidArgument(format!(
                            "line {lineno}: `split` needs a branch name"
                        ))
                    })?
                    .to_string();
                segments.push(Segment {
                    branch: Some(name),
                    picks: Vec::new(),
                });
            }
            other => {
                return Err(OakError::InvalidArgument(format!(
                    "line {lineno}: unknown plan keyword '{other}' (expected pick/drop/split)"
                )));
            }
        }
    }

    // Every original commit must be accounted for, so nothing is silently lost.
    for c in &history.commits {
        if !seen.contains(&c.hash.to_string()) {
            return Err(OakError::InvalidArgument(format!(
                "commit {} is missing from the plan — `pick` or `drop` it explicitly",
                c.hash.short()
            )));
        }
    }
    if segments.iter().all(|s| s.picks.is_empty()) {
        return Err(OakError::InvalidArgument(
            "plan drops every commit — nothing left to keep".to_string(),
        ));
    }
    Ok(Plan { segments })
}

// ---------------------------------------------------------------------------
// Replay engine (pure)
// ---------------------------------------------------------------------------

/// A replay conflict: the commit being replayed and the paths it collided on.
#[derive(Debug)]
struct ReplayConflict {
    paths: Vec<String>,
}

/// Replay a sequence of patches onto `base`. Each patch is
/// `(original_parent_manifest, commit_manifest)`: the change a commit
/// introduced relative to *its* parent. We carry that change onto the running
/// tree with a 3-way merge, so reordered / partial sequences compose correctly
/// and a dependency that isn't present surfaces as a conflict.
///
/// Returns the resulting manifest after each step (same length as `patches`),
/// or the first conflict encountered.
fn replay_onto(
    base: &Manifest,
    patches: &[(Manifest, Manifest)],
) -> std::result::Result<Vec<Manifest>, ReplayConflict> {
    let mut running = base.clone();
    let mut out = Vec::with_capacity(patches.len());
    for (parent_manifest, commit_manifest) in patches {
        let outcome = three_way_merge_manifests(parent_manifest, commit_manifest, &running);
        if !outcome.conflicts.is_empty() {
            let mut paths: Vec<String> = outcome.conflicts.iter().map(|c| c.path.clone()).collect();
            paths.sort();
            return Err(ReplayConflict { paths });
        }
        running = Manifest::new(outcome.clean_entries);
        out.push(running.clone());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Result of applying a plan: `(branch_name, new_tip)` per non-empty segment.
fn apply_plan(
    lock: &crate::workdir_lock::WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    source_branch: &str,
    history: &BranchHistory,
    plan: &Plan,
    update_worktree: bool,
) -> Result<Vec<(String, Hash)>> {
    // Validate split targets up front so we fail before writing anything.
    for seg in &plan.segments {
        if let Some(name) = &seg.branch {
            if name == DEFAULT_BRANCH {
                return Err(OakError::InvalidArgument(
                    "a split branch can't be named 'main'".to_string(),
                ));
            }
            if name == source_branch || repo.get_branch(name)?.is_some() {
                return Err(OakError::BranchAlreadyExists(name.clone()));
            }
            if seg.picks.is_empty() {
                return Err(OakError::InvalidArgument(format!(
                    "split branch '{name}' would have no commits"
                )));
            }
        }
    }

    let mut results: Vec<(String, Hash)> = Vec::new();
    let mut source_tip: Option<Hash> = None;

    for (idx, seg) in plan.segments.iter().enumerate() {
        if seg.picks.is_empty() {
            continue;
        }
        let target = seg
            .branch
            .clone()
            .unwrap_or_else(|| source_branch.to_string());

        // Build the patch list for this segment from stored manifests.
        let mut patches: Vec<(Manifest, Manifest)> = Vec::with_capacity(seg.picks.len());
        for h in &seg.picks {
            let commit = repo
                .get_commit(h)?
                .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
            let commit_manifest = repo
                .get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(Manifest::empty);
            let parent_manifest = match &commit.parent_hash {
                Some(ph) => {
                    let p = repo
                        .get_commit(ph)?
                        .ok_or_else(|| OakError::CommitNotFound(ph.to_string()))?;
                    repo.get_manifest(&p.manifest_hash)?
                        .unwrap_or_else(Manifest::empty)
                }
                None => Manifest::empty(),
            };
            patches.push((parent_manifest, commit_manifest));
        }

        // Replay onto the branch's original base — independent of other segments.
        let manifests = replay_onto(&history.base_manifest, &patches).map_err(|c| {
            OakError::InvalidArgument(format!(
                "branch '{target}' can't be built independently: replay conflicts on {}. \
                 These commits depend on changes in another segment — keep dependent commits \
                 in the same branch, or order them so each builds on its own segment.",
                c.paths.join(", ")
            ))
        })?;

        // Create the branch row for split targets (parented on the trunk).
        if seg.branch.is_some() {
            let description = format!("Split from {source_branch}");
            let b = Branch::new(
                target.clone(),
                Some(description),
                Some(DEFAULT_BRANCH.to_string()),
            );
            repo.store_branch(&b)?;
        }

        // Write the rewritten commits, preserving author + timestamp.
        let mut running = history.base_manifest.clone();
        let mut parent = history.base_commit.clone();
        for (h, new_manifest) in seg.picks.iter().zip(manifests.iter()) {
            let original = repo
                .get_commit(h)?
                .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
            let files = running.diff(new_manifest);
            repo.store_manifest(new_manifest)?;
            let commit = Commit::with_timestamp(
                target.clone(),
                parent.clone(),
                None,
                new_manifest.hash.clone(),
                original.author.clone(),
                None,
                files,
                original.timestamp,
            )?;
            repo.store_commit(&commit)?;
            parent = Some(commit.hash.clone());
            running = new_manifest.clone();
        }

        let tip = parent.expect("segment had >=1 pick, so a tip exists");
        repo.set_branch_head(&target, &tip)?;
        if idx == 0 && seg.branch.is_none() {
            source_tip = Some(tip.clone());
        }
        results.push((target, tip));
    }

    // If we rewrote the branch the working tree is on, move the tree to the new
    // tip so it isn't left pointing at orphaned history.
    if update_worktree {
        if let Some(tip) = &source_tip {
            let commit = repo
                .get_commit(tip)?
                .ok_or_else(|| OakError::CommitNotFound(tip.to_string()))?;
            let manifest = repo
                .get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(Manifest::empty);
            super::switch::update_working_dir(lock, root, repo, &manifest)?;
            repo.set_head(tip)?;
        }
    }

    Ok(results)
}

/// Print what a plan would do, for `--dry-run`.
fn print_plan_summary(source_branch: &str, history: &BranchHistory, plan: &Plan) {
    output::info(&format!(
        "split of '{source_branch}' ({} commit(s) on base {})",
        history.commits.len(),
        history
            .base_commit
            .as_ref()
            .map(|h| h.short())
            .unwrap_or("<root>"),
    ));
    for (idx, seg) in plan.segments.iter().enumerate() {
        if seg.picks.is_empty() {
            continue;
        }
        let name = seg
            .branch
            .clone()
            .unwrap_or_else(|| format!("{source_branch} (rewritten)"));
        let kind = if idx == 0 {
            "branch"
        } else {
            "new branch off main"
        };
        output::print_line(&format!("  {kind}: {name}"));
        for h in &seg.picks {
            if let Some(c) = history.commits.iter().find(|c| &c.hash == h) {
                output::print_line(&format!("      pick {} {}", c.hash.short(), summarize(c)));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `oak split` — see module docs.
pub fn run(path: &Path, from: Option<&str>, plan_path: Option<&str>, dry_run: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let root = ctx.work_tree.clone();
    let repo = ctx.open()?;
    let repo_ref = repo.as_ref();

    let editing_current = from.is_none();
    let branch = match from {
        Some(b) => b.to_string(),
        None => repo_ref.get_current_branch_name()?.ok_or_else(|| {
            OakError::InvalidArgument(
                "not on a branch (detached HEAD) — pass --from <branch>".to_string(),
            )
        })?,
    };
    if branch == DEFAULT_BRANCH {
        return Err(OakError::InvalidArgument(
            "'main' has no editable local history".to_string(),
        ));
    }

    // History rewrite + working-tree move: refuse to clobber uncommitted work.
    let (changes, _, _) = super::commit::get_status(path)?;
    if !changes.is_empty() {
        return Err(OakError::UncommittedChanges);
    }

    let history = load_history(repo_ref, &branch)?;

    let plan_text = match plan_path {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(OakError::Io)?;
            s
        }
        Some(p) => std::fs::read_to_string(p).map_err(OakError::Io)?,
        None => {
            // The interactive plan editor draws through crossterm straight to
            // stdout, bypassing the broken-pipe-safe output funnel. This hard
            // gate keeps that bypass provably unreachable when stdout is not
            // a terminal (`oak split | head` must never enter raw mode or
            // write escape sequences into a pipe).
            if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                return Err(OakError::InvalidArgument(
                    "oak split's interactive editor needs a terminal; use `oak split --plan <file|->` when stdout is not a TTY".to_string(),
                ));
            }
            match tui::edit_plan(&branch, &history)? {
                Some(text) => text,
                None => {
                    output::info("Split cancelled — nothing changed.");
                    return Ok(());
                }
            }
        }
    };

    let plan = parse_plan(&plan_text, &history)?;

    if dry_run {
        print_plan_summary(&branch, &history, &plan);
        output::info("(dry run — nothing written)");
        return Ok(());
    }

    let lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
    let results = apply_plan(
        &lock,
        repo_ref,
        &root,
        &branch,
        &history,
        &plan,
        editing_current,
    )?;
    let split_count = results.len().saturating_sub(1);
    output::success(&format!(
        "Rewrote '{branch}'{}.",
        if split_count > 0 {
            format!(" and created {split_count} split branch(es)")
        } else {
            String::new()
        }
    ));
    for (name, tip) in &results {
        output::item(&format!("{name} → {}", tip.short()));
    }
    output::info(
        "Review with `oak log`. The source branch was rewritten — push it with `oak push --force`.",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive TUI
// ---------------------------------------------------------------------------

mod tui {
    use std::io;

    use crossterm::{
        cursor::Show,
        event::{self, Event, KeyCode, KeyEventKind},
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    };
    use oak_core::Result;
    use ratatui::{
        prelude::*,
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    };

    use super::{summarize, BranchHistory};

    /// One row in the editor: a commit (with its dropped state) or a split
    /// boundary that opens a new branch.
    enum Row {
        Commit { idx: usize, dropped: bool },
        Split { name: String },
    }

    struct Editor {
        source: String,
        history_len: usize,
        rows: Vec<Row>,
        cursor: usize,
        confirmed: bool,
        cancelled: bool,
    }

    impl Editor {
        fn new(source: &str, history: &BranchHistory) -> Self {
            let rows = (0..history.commits.len())
                .map(|idx| Row::Commit {
                    idx,
                    dropped: false,
                })
                .collect();
            Editor {
                source: source.to_string(),
                history_len: history.commits.len(),
                rows,
                cursor: 0,
                confirmed: false,
                cancelled: false,
            }
        }

        fn move_cursor(&mut self, delta: isize) {
            if self.rows.is_empty() {
                return;
            }
            let max = self.rows.len() as isize - 1;
            let next = (self.cursor as isize + delta).clamp(0, max);
            self.cursor = next as usize;
        }

        /// Move the row under the cursor up or down by one.
        fn shift_row(&mut self, delta: isize) {
            let from = self.cursor as isize;
            let to = from + delta;
            if to < 0 || to >= self.rows.len() as isize {
                return;
            }
            self.rows.swap(from as usize, to as usize);
            self.cursor = to as usize;
        }

        fn toggle_drop(&mut self) {
            if let Some(Row::Commit { dropped, .. }) = self.rows.get_mut(self.cursor) {
                *dropped = !*dropped;
            }
        }

        /// Count existing split rows, to auto-name the next one.
        fn split_count(&self) -> usize {
            self.rows
                .iter()
                .filter(|r| matches!(r, Row::Split { .. }))
                .count()
        }

        /// Insert (or remove) a split boundary above the cursor.
        fn toggle_split(&mut self) {
            if matches!(self.rows.get(self.cursor), Some(Row::Split { .. })) {
                self.rows.remove(self.cursor);
                if self.cursor >= self.rows.len() && self.cursor > 0 {
                    self.cursor -= 1;
                }
                return;
            }
            let name = format!("{}-part{}", self.source, self.split_count() + 2);
            self.rows.insert(self.cursor, Row::Split { name });
        }

        /// Emit the plan text the engine parses.
        fn to_plan(&self, history: &BranchHistory) -> String {
            let mut out = String::new();
            for row in &self.rows {
                match row {
                    Row::Commit { idx, dropped } => {
                        let c = &history.commits[*idx];
                        let kw = if *dropped { "drop" } else { "pick" };
                        out.push_str(&format!("{kw} {}\n", c.hash.short()));
                    }
                    Row::Split { name } => {
                        out.push_str(&format!("split {name}\n"));
                    }
                }
            }
            out
        }
    }

    struct Guard;
    impl Guard {
        fn enter() -> Result<Self> {
            enable_raw_mode()?;
            io::stdout().execute(EnterAlternateScreen)?;
            Ok(Self)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = io::stdout().execute(LeaveAlternateScreen);
            let _ = io::stdout().execute(Show);
            let _ = disable_raw_mode();
        }
    }

    /// Run the editor. Returns `Some(plan_text)` on confirm, `None` on cancel.
    pub fn edit_plan(source: &str, history: &BranchHistory) -> Result<Option<String>> {
        let _guard = Guard::enter()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        let mut ed = Editor::new(source, history);

        while !ed.confirmed && !ed.cancelled {
            terminal.draw(|f| draw(f, &ed, history))?;
            if event::poll(std::time::Duration::from_millis(150))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => ed.cancelled = true,
                        KeyCode::Enter => ed.confirmed = true,
                        KeyCode::Up | KeyCode::Char('k') => ed.move_cursor(-1),
                        KeyCode::Down | KeyCode::Char('j') => ed.move_cursor(1),
                        KeyCode::Char('K') => ed.shift_row(-1),
                        KeyCode::Char('J') => ed.shift_row(1),
                        KeyCode::Char('x') | KeyCode::Char(' ') => ed.toggle_drop(),
                        KeyCode::Char('s') => ed.toggle_split(),
                        _ => {}
                    }
                }
            }
        }

        if ed.confirmed {
            Ok(Some(ed.to_plan(history)))
        } else {
            Ok(None)
        }
    }

    fn draw(f: &mut Frame, ed: &Editor, history: &BranchHistory) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(f.area());

        let header = Paragraph::new(format!(
            "split {}  —  {} commit(s) on base {}",
            ed.source,
            ed.history_len,
            history
                .base_commit
                .as_ref()
                .map(|h| h.short())
                .unwrap_or("<root>"),
        ))
        .style(Style::default().add_modifier(Modifier::BOLD));
        f.render_widget(header, chunks[0]);

        let mut seg = 0usize;
        let items: Vec<ListItem> = ed
            .rows
            .iter()
            .map(|row| match row {
                Row::Split { name } => {
                    seg += 1;
                    ListItem::new(Line::from(vec![Span::styled(
                        format!("── split → new branch off main: {name}"),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    )]))
                }
                Row::Commit { idx, dropped } => {
                    let c = &history.commits[*idx];
                    let label = format!("{} {}", c.hash.short(), summarize(c));
                    let (kw, style) = if *dropped {
                        (
                            "drop",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::CROSSED_OUT),
                        )
                    } else {
                        ("pick", Style::default())
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("  [{seg}] "), Style::default().fg(Color::DarkGray)),
                        Span::styled(format!("{kw} "), Style::default().fg(Color::Green)),
                        Span::styled(label, style),
                    ]))
                }
            })
            .collect();

        let mut state = ListState::default();
        if !ed.rows.is_empty() {
            state.select(Some(ed.cursor));
        }
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" commits "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, chunks[1], &mut state);

        let help = Paragraph::new(
            "↑/↓ move  ·  J/K reorder  ·  x drop  ·  s split here  ·  Enter apply  ·  q cancel",
        )
        .style(Style::default().fg(Color::DarkGray));
        f.render_widget(help, chunks[2]);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{FileMode, ManifestEntry};

    fn man(files: &[(&str, &str)]) -> Manifest {
        Manifest::new(
            files
                .iter()
                .map(|(p, h)| ManifestEntry {
                    path: p.to_string(),
                    blob_hash: Hash(h.to_string()),
                    mode: FileMode::Regular,
                })
                .collect(),
        )
    }

    #[test]
    fn replay_disjoint_segments_compose() {
        let base = man(&[("a", "1")]);
        // commit1 adds b; commit2 adds c — independent.
        let patches = [
            (man(&[("a", "1")]), man(&[("a", "1"), ("b", "1")])),
            (
                man(&[("a", "1"), ("b", "1")]),
                man(&[("a", "1"), ("b", "1"), ("c", "1")]),
            ),
        ];
        let out = replay_onto(&base, &patches).expect("disjoint patches replay cleanly");
        let last = out.last().unwrap();
        assert!(last.get("b").is_some());
        assert!(last.get("c").is_some());
    }

    #[test]
    fn replay_reorder_independent_changes() {
        let base = man(&[]);
        // Apply commit2 (adds y) before commit1 (adds x): order-independent adds.
        let patches = [
            (man(&[]), man(&[("y", "1")])),
            (man(&[]), man(&[("x", "1")])),
        ];
        let out = replay_onto(&base, &patches).expect("independent adds reorder cleanly");
        let last = out.last().unwrap();
        assert!(last.get("x").is_some() && last.get("y").is_some());
    }

    #[test]
    fn replay_dependent_segment_conflicts() {
        // A segment that modifies `b` (parent had b=1 → b=2) replayed onto a base
        // where `b` doesn't exist must conflict: it isn't independent off main.
        let base = man(&[("a", "1")]);
        let patches = [(
            man(&[("a", "1"), ("b", "1")]),
            man(&[("a", "1"), ("b", "2")]),
        )];
        let err = replay_onto(&base, &patches).expect_err("dependent change conflicts");
        assert!(err.paths.contains(&"b".to_string()));
    }
}
