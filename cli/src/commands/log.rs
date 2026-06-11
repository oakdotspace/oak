use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::path::Path;

use oak_core::Repository;
use oak_core::{Commit, FileDiff, Hash, Result};

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
pub fn run(path: &Path, limit: Option<usize>, verbose: bool) -> Result<()> {
    let work_path = path.to_path_buf();
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    let piped = !io::stdout().is_terminal();

    // Effective limit: explicit `-n` always wins; piped output defaults to
    // a window; the interactive TUI browses everything.
    let effective_limit = match limit {
        Some(n) => Some(n),
        None if piped => Some(DEFAULT_PIPED_LIMIT),
        None => None,
    };

    // Walk one commit past the limit so "is there more?" is answerable
    // without walking (or loading) the entire history.
    let walk_cap = effective_limit.map(|n| n.saturating_add(1));

    // Walk the commit graph from the branch's effective head via parent_hash.
    // Filtering by `commits.branch_name` would miss reachable history on a
    // personal branch created off another branch's tip, since the older
    // commits carry their original branch_name. This walk matches what
    // `oak status` and `oak commit` see at HEAD.
    let commits = match &branch_name {
        Some(name) => walk_history_from_branch(repo.as_ref(), name, walk_cap)?,
        None => {
            let mut all = repo.get_all_commits()?;
            all.reverse();
            if let Some(cap) = walk_cap {
                all.truncate(cap);
            }
            all
        }
    };

    if commits.is_empty() {
        output::info("No commits yet");
        return Ok(());
    }

    // If stdout is not a TTY, fall back to plain-text output: compact
    // one-line-per-commit by default, the detailed block with `-v`.
    if piped {
        let shown = effective_limit.unwrap_or(commits.len());
        let truncated = commits.len() > shown;
        if verbose {
            for commit in commits.iter().take(shown) {
                output::print_line(&output::format_commit_compact_verbose(commit));
            }
        } else {
            for commit in commits.iter().take(shown) {
                output::print_line(&output::format_commit_compact(commit));
            }
        }
        if truncated {
            output::print_line(&output::format_log_more_hint(shown));
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
pub fn run_json(path: &Path, limit: Option<usize>) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch_name = repo.get_current_branch_name().ok().flatten();

    let commits = match &branch_name {
        Some(name) => walk_history_from_branch(repo.as_ref(), name, limit)?,
        None => {
            let mut all = repo.get_all_commits()?;
            all.reverse();
            if let Some(limit) = limit {
                all.truncate(limit);
            }
            all
        }
    };

    let json: Vec<output::LogCommitJson> = commits
        .iter()
        .map(|commit| output::LogCommitJson {
            hash: commit.hash.to_string(),
            timestamp: commit.timestamp.to_rfc3339(),
            branch: commit.branch_name.clone(),
            description_or_subject: output::commit_description_or_subject(commit),
            files_changed: commit.files.len(),
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
fn walk_history_from_branch(
    repo: &dyn Repository,
    branch_name: &str,
    cap: Option<usize>,
) -> Result<Vec<Commit>> {
    let head = resolve_effective_head(repo, branch_name)?;
    let mut commits = Vec::new();
    let mut seen: HashSet<Hash> = HashSet::new();
    let mut cur = head;
    while let Some(hash) = cur {
        if cap.is_some_and(|cap| commits.len() >= cap) {
            break;
        }
        if !seen.insert(hash.clone()) {
            break;
        }
        match repo.get_commit(&hash)? {
            Some(commit) => {
                cur = commit.parent_hash.clone();
                commits.push(commit);
            }
            None => {
                // Commit in parent chain is missing from local storage — stop
                // the walk rather than erroring.  This can happen when the
                // branch head was pinned to a server commit that was never
                // downloaded (e.g. after a clone of an empty repo that later
                // received pushes before the first `oak pull`).  Run
                // `oak pull` to bring local storage up to date.
                output::warning(&format!(
                    "History truncated: commit {} not in local storage (run 'oak pull' to sync)",
                    &hash.0[..12.min(hash.0.len())]
                ));
                break;
            }
        }
    }
    Ok(commits)
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

    let mut file_diffs = Vec::new();
    for path in all_paths {
        let old_hash = old_entries.get(path).map(|e| &e.blob_hash);
        let new_hash = new_entries.get(path).map(|e| &e.blob_hash);
        if old_hash == new_hash {
            continue;
        }
        let old_content = if let Some(hash) = old_hash {
            repo.get_blob(hash)?
                .map(|b| String::from_utf8_lossy(&b.content).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let new_content = if let Some(hash) = new_hash {
            repo.get_blob(hash)?
                .map(|b| String::from_utf8_lossy(&b.content).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let diff = FileDiff::new(path, &old_content, &new_content);
        if diff.has_changes {
            file_diffs.push(diff);
        }
    }

    let mut lines = Vec::new();
    if file_diffs.is_empty() {
        lines.push(Line::from(Span::styled(
            "No differences to display",
            Style::default().fg(Color::DarkGray),
        )));
        return Ok(lines);
    }

    for diff in &file_diffs {
        for line in diff.to_unified().lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                style_diff_line(line),
            )));
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
