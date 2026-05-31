use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::path::Path;

use oak_core::Repository;
use oak_core::{Commit, FileDiff, Hash, Result};

use crate::output;
use crate::resolve::RepoContext;

// Re-use switch logic
use super::switch;

/// Show the commit history for the current branch
pub fn run(path: &Path, limit: Option<usize>, verbose: bool) -> Result<()> {
    let work_path = path.to_path_buf();
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    // Walk the commit graph from the branch's effective head via parent_hash.
    // Filtering by `commits.branch_name` would miss reachable history on a
    // personal branch created off another branch's tip, since the older
    // commits carry their original branch_name. This walk matches what
    // `oak status` and `oak commit` see at HEAD.
    let commits = match &branch_name {
        Some(name) => walk_history_from_branch(repo.as_ref(), name)?,
        None => {
            let mut all = repo.get_all_commits()?;
            all.reverse();
            all
        }
    };

    if commits.is_empty() {
        output::info("No commits yet");
        return Ok(());
    }

    // Apply limit
    let commits: Vec<_> = if let Some(n) = limit {
        commits.into_iter().take(n).collect()
    } else {
        commits
    };

    // If stdout is not a TTY, fall back to plain-text output
    if !io::stdout().is_terminal() {
        if let Some(ref name) = branch_name {
            output::info(&format!("Commits for branch '{name}':\n"));
        }
        for commit in commits {
            output::print_line(&output::format_commit(&commit, verbose));
        }
        return Ok(());
    }

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

/// Walk the commit graph backward from a branch's effective head, returning
/// commits in newest-first order. The effective head falls back through
/// `parent_branch` when the branch itself has no head yet, mirroring
/// `commit::resolve_effective_head`. Cycle-guarded via a seen set.
fn walk_history_from_branch(repo: &dyn Repository, branch_name: &str) -> Result<Vec<Commit>> {
    let head = resolve_effective_head(repo, branch_name)?;
    let mut commits = Vec::new();
    let mut seen: HashSet<Hash> = HashSet::new();
    let mut cur = head;
    while let Some(hash) = cur {
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

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

enum View {
    List,
    Detail(usize),
    Diff(DiffViewState),
}

struct DiffViewState {
    commit_idx: usize,
    diff_lines: Vec<DiffDisplayLine>,
    scroll: usize,
}

/// A single line of formatted diff output for the TUI
struct DiffDisplayLine {
    text: String,
    style: Style,
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
    list_state: ListState,
    view: View,
    search_active: bool,
    search_query: String,
    filtered_indices: Vec<usize>,
    quit: bool,
    action: TuiAction,
}

impl App {
    fn new(commits: Vec<Commit>, branch_name: Option<String>, ctx: RepoContext) -> Self {
        let filtered_indices: Vec<usize> = (0..commits.len()).collect();
        let mut list_state = ListState::default();
        if !filtered_indices.is_empty() {
            list_state.select(Some(0));
        }
        App {
            commits,
            branch_name,
            ctx,
            list_state,
            view: View::List,
            search_active: false,
            search_query: String::new(),
            filtered_indices,
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
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    fn selected_commit_index(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|sel| self.filtered_indices.get(sel).copied())
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let len = self.filtered_indices.len();
        let current = self.list_state.selected().unwrap_or(0) as i32;
        let next = current.saturating_add(delta).clamp(0, len as i32 - 1) as usize;
        self.list_state.select(Some(next));
    }

    fn select_first(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.list_state
                .select(Some(self.filtered_indices.len() - 1));
        }
    }

    /// Compute the diff for a commit (comparing against its parent)
    fn compute_diff(&self, commit_idx: usize) -> DiffViewState {
        let commit = &self.commits[commit_idx];
        let mut lines = Vec::new();

        let result = (|| -> Result<Vec<DiffDisplayLine>> {
            let repo = self.ctx.open()?;
            let mut diff_lines = Vec::new();

            // Get the commit's manifest
            let manifest = repo
                .get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(oak_core::Manifest::empty);

            // Get the parent manifest (empty if first commit)
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

            // Build lookup maps
            let old_entries: std::collections::HashMap<&str, &oak_core::ManifestEntry> =
                parent_manifest
                    .entries
                    .iter()
                    .map(|e| (e.path.as_str(), e))
                    .collect();
            let new_entries: std::collections::HashMap<&str, &oak_core::ManifestEntry> = manifest
                .entries
                .iter()
                .map(|e| (e.path.as_str(), e))
                .collect();

            // Collect all unique paths, sorted
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

                // Skip unchanged files
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

            if file_diffs.is_empty() {
                diff_lines.push(DiffDisplayLine {
                    text: "No differences to display".to_string(),
                    style: Style::default().fg(Color::DarkGray),
                });
            } else {
                for diff in &file_diffs {
                    let unified = diff.to_unified();
                    for line in unified.lines() {
                        let style = if line.starts_with('+') && !line.starts_with("+++") {
                            Style::default().fg(Color::Green)
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            Style::default().fg(Color::Red)
                        } else if line.starts_with("@@") {
                            Style::default().fg(Color::Cyan)
                        } else if line.starts_with("diff ")
                            || line.starts_with("--- ")
                            || line.starts_with("+++ ")
                        {
                            Style::default().bold()
                        } else {
                            Style::default()
                        };
                        diff_lines.push(DiffDisplayLine {
                            text: line.to_string(),
                            style,
                        });
                    }
                    // Blank line between files
                    diff_lines.push(DiffDisplayLine {
                        text: String::new(),
                        style: Style::default(),
                    });
                }
            }

            Ok(diff_lines)
        })();

        match result {
            Ok(l) => lines = l,
            Err(e) => {
                lines.push(DiffDisplayLine {
                    text: format!("Error computing diff: {e}"),
                    style: Style::default().fg(Color::Red),
                });
            }
        }

        DiffViewState {
            commit_idx,
            diff_lines: lines,
            scroll: 0,
        }
    }
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
        terminal.draw(|f| draw_ui(f, &mut app))?;

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
            KeyCode::Esc | KeyCode::Enter => {
                app.search_active = false;
            }
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

    match &mut app.view {
        View::List => match key {
            KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
            KeyCode::Char('/') => {
                app.search_active = true;
            }
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
            KeyCode::Enter | KeyCode::Right => {
                if let Some(idx) = app.selected_commit_index() {
                    app.view = View::Detail(idx);
                }
            }
            KeyCode::Char('c') => {
                if let Some(idx) = app.selected_commit_index() {
                    let hash = app.commits[idx].hash.to_string();
                    app.action = TuiAction::Checkout(hash);
                    app.quit = true;
                }
            }
            KeyCode::Char('r') => {
                app.action = TuiAction::CheckoutHead;
                app.quit = true;
            }
            _ => {}
        },
        View::Detail(idx) => {
            let idx = *idx;
            match key {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                    app.view = View::List;
                }
                KeyCode::Char('d') => {
                    let diff_state = app.compute_diff(idx);
                    app.view = View::Diff(diff_state);
                }
                KeyCode::Char('c') => {
                    let hash = app.commits[idx].hash.to_string();
                    app.action = TuiAction::Checkout(hash);
                    app.quit = true;
                }
                KeyCode::Char('r') => {
                    app.action = TuiAction::CheckoutHead;
                    app.quit = true;
                }
                _ => {}
            }
        }
        View::Diff(ref mut state) => match key {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                let idx = state.commit_idx;
                app.view = View::Detail(idx);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if state.scroll > 0 {
                    state.scroll -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.scroll = state.scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                state.scroll = state.scroll.saturating_sub(20);
            }
            KeyCode::PageDown => {
                state.scroll = state.scroll.saturating_add(20);
            }
            KeyCode::Char('b') if ctrl => {
                state.scroll = state.scroll.saturating_sub(20);
            }
            KeyCode::Char('f') if ctrl => {
                state.scroll = state.scroll.saturating_add(20);
            }
            KeyCode::Char('u') if ctrl => {
                state.scroll = state.scroll.saturating_sub(10);
            }
            KeyCode::Char('d') if ctrl => {
                state.scroll = state.scroll.saturating_add(10);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                state.scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                state.scroll = state.diff_lines.len().saturating_sub(1);
            }
            _ => {}
        },
    }
}

fn draw_ui(f: &mut Frame, app: &mut App) {
    match &app.view {
        View::List => draw_list(f, app),
        View::Detail(idx) => {
            let idx = *idx;
            draw_detail(f, app, idx);
        }
        View::Diff(_) => draw_diff(f, app),
    }
}

fn draw_list(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let show_search = app.search_active || !app.search_query.is_empty();
    let chunks = if show_search {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area)
    };

    let selected_pos = app.list_state.selected().map(|s| s + 1).unwrap_or(0);
    let total = app.filtered_indices.len();
    let title = match &app.branch_name {
        Some(name) => format!(" oak log · branch '{}' · {}/{} ", name, selected_pos, total),
        None => format!(" oak log · {}/{} ", selected_pos, total),
    };

    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&i| {
            let c = &app.commits[i];
            let line = format!(
                "{} {} {} - {}",
                c.hash.short(),
                c.author,
                c.timestamp.format("%Y-%m-%d %H:%M"),
                c.message.as_deref().unwrap_or("(no message)"),
            );
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("\u{25b8} ");

    f.render_stateful_widget(list, chunks[0], &mut app.list_state);

    if show_search {
        let search_text = format!("/{}", app.search_query);
        let style = if app.search_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let search = Paragraph::new(search_text)
            .style(style)
            .block(Block::default().borders(Borders::ALL).title(" Search "));
        f.render_widget(search, chunks[1]);
    }

    let help = Paragraph::new(Line::from(Span::styled(
        " ↑↓/jk  move  ·  PgUp/PgDn/^F/^B  page  ·  ^U/^D  half  ·  g/G  top/bottom  ·  Enter  detail  ·  c  switch  ·  r  HEAD  ·  /  search  ·  q  quit",
        Style::default().fg(Color::DarkGray),
    )));
    let help_idx = if show_search { 2 } else { 1 };
    f.render_widget(help, chunks[help_idx]);
}

fn draw_detail(f: &mut Frame, app: &App, idx: usize) {
    let area = f.area();
    let commit = &app.commits[idx];

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("commit:    ", Style::default().fg(Color::Yellow).bold()),
            Span::raw(commit.hash.to_string()),
        ]),
        Line::from(vec![
            Span::styled("branch:    ", Style::default().fg(Color::Yellow).bold()),
            Span::raw(&commit.branch_name),
        ]),
        Line::from(vec![
            Span::styled("author:    ", Style::default().fg(Color::Yellow).bold()),
            Span::raw(&commit.author),
        ]),
        Line::from(vec![
            Span::styled("date:      ", Style::default().fg(Color::Yellow).bold()),
            Span::raw(commit.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::raw(commit.message.as_deref().unwrap_or("(no message)")),
        ]),
    ];

    if !commit.files.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Files changed:",
            Style::default().bold(),
        )));

        for file in &commit.files {
            let (prefix, color) = match file.change_type {
                oak_core::ChangeType::Added => ("A", Color::Green),
                oak_core::ChangeType::Modified => ("M", Color::Yellow),
                oak_core::ChangeType::Deleted => ("D", Color::Red),
                oak_core::ChangeType::Renamed => ("R", Color::Cyan),
            };
            let display_path = if file.change_type == oak_core::ChangeType::Renamed {
                if let Some(ref old_path) = file.old_path {
                    format!("{} -> {}", old_path, file.path)
                } else {
                    file.path.clone()
                }
            } else {
                file.path.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {prefix} "), Style::default().fg(color)),
                Span::raw(display_path),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ← / Esc  back  ·  d  view diff  ·  c  switch to this commit  ·  r  return to HEAD",
        Style::default().fg(Color::DarkGray),
    )));

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Commit Detail "),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(Clear, area);
    f.render_widget(detail, area);
}

fn draw_diff(f: &mut Frame, app: &App) {
    let area = f.area();

    let state = match &app.view {
        View::Diff(s) => s,
        _ => return,
    };

    let commit = &app.commits[state.commit_idx];
    let title = format!(
        " Diff · {} · {} ",
        commit.hash.short(),
        commit.message.as_deref().unwrap_or("(no message)")
    );

    // Reserve 1 line at the bottom for the help bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let content_height = chunks[0].height.saturating_sub(2) as usize; // minus border

    // Clamp scroll
    let max_scroll = state.diff_lines.len().saturating_sub(content_height);
    let scroll = state.scroll.min(max_scroll);

    let lines: Vec<Line> = state
        .diff_lines
        .iter()
        .skip(scroll)
        .take(content_height)
        .map(|dl| Line::from(Span::styled(&dl.text, dl.style)))
        .collect();

    let diff_widget =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));

    let help = Paragraph::new(Line::from(Span::styled(
        " ← / Esc  back  ·  ↑↓/jk  scroll  ·  PgUp/PgDn/^F/^B  page  ·  ^U/^D  half  ·  g/G  top/bottom",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Clear, area);
    f.render_widget(diff_widget, chunks[0]);
    f.render_widget(help, chunks[1]);
}
