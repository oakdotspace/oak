use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use oak_core::Repository;
use oak_core::{ChangeType, DiffLine, FileChange, FileDiff, FileMode, Hash, ManifestEntry, Result};

use crate::output;

/// Show diff between working directory and HEAD.
///
/// The set of paths shown here is computed by the same scan/diff pipeline that
/// `oak status` uses and `oak commit` records with. `oak diff` takes the
/// non-persisting path because hunk rendering reads new bytes directly from the
/// worktree; it only needs committed pre-image blobs from the object store.
/// Status, diff, and commit must still agree on *which* files changed. In
/// particular, diff must never silently omit a path just because it can't render
/// a textual hunk for it (binary files, a trailing-newline-only change, a pure
/// mode change): it emits a "differ" notice instead. (It used to gate every file
/// on `FileDiff::has_changes`, which dropped binary edits that status/commit
/// still counted.)
pub fn run(path: &Path, paths: &[std::path::PathBuf], stat: bool, name_only: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    let filters = resolve_filters(path, &work_tree, paths)?;
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo.as_ref(), &work_tree)?;
    // Before filtering, so a path-scoped diff still surfaces a mass drop of
    // tracked-but-now-ignored files elsewhere in the tree.
    super::commit::warn_tracked_now_ignored(
        &changes,
        &work_tree,
        "will be removed from the branch by the next commit",
    );
    let changes = filter_changes(changes, &filters);
    if changes.is_empty() {
        output::info(no_differences_message(&filters));
        return Ok(());
    }

    if name_only {
        for line in render_name_only(&changes) {
            output::print_line(&line);
        }
        return Ok(());
    }

    let activity = output::activity("Loading HEAD manifest...");
    let head_manifest = head_manifest_for(repo.as_ref(), &head)?;
    let head_files = manifest_file_map(&head_manifest);

    if stat {
        activity.set_message("Computing diff statistics...");
        let rows = render_stat_with_worktree_blobs(
            repo.as_ref(),
            &work_tree,
            &head_files,
            &changes,
            &worktree_blobs,
        )?;
        activity.finish_and_clear();
        for line in rows {
            output::print_line(&line);
        }
        return Ok(());
    }

    activity.finish_and_clear();
    print_changes(
        repo.as_ref(),
        &work_tree,
        &head_files,
        &changes,
        &worktree_blobs,
    )
}

pub(crate) fn render_name_only(changes: &[FileChange]) -> Vec<String> {
    changes.iter().map(|change| change.path.clone()).collect()
}

/// The "nothing to show" line, qualified when the user scoped the diff so an
/// empty result is distinguishable from a clean tree.
fn no_differences_message(filters: &[String]) -> &'static str {
    if filters.is_empty() {
        "No differences"
    } else {
        "No differences in the given paths"
    }
}

/// Translate user-supplied CLI paths (absolute or cwd-relative) into
/// repo-root-relative forward-slash filter strings. Errors if a path lies
/// outside the repository.
pub(crate) fn resolve_filters(
    cwd: &Path,
    work_tree: &Path,
    paths: &[std::path::PathBuf],
) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| crate::pathutil::repo_relative_str(cwd, work_tree, &p.to_string_lossy()))
        .collect()
}

/// Does `path` (repo-relative, forward slashes) match any filter — either
/// exactly, or as a file under a filter directory? An empty filter (the repo
/// root itself, e.g. `oak diff .`) matches everything.
pub(crate) fn path_matches(path: &str, filters: &[String]) -> bool {
    filters.iter().any(|f| {
        let f = f.trim_end_matches('/');
        f.is_empty() || path == f || (path.starts_with(f) && path.as_bytes()[f.len()] == b'/')
    })
}

/// Restrict a change set to the user's paths. With no filters the set passes
/// through untouched; renames match on either their old or new path so a
/// scoped diff never hides where a file went.
pub(crate) fn filter_changes(changes: Vec<FileChange>, filters: &[String]) -> Vec<FileChange> {
    if filters.is_empty() {
        return changes;
    }
    changes
        .into_iter()
        .filter(|c| {
            path_matches(&c.path, filters)
                || c.old_path
                    .as_deref()
                    .is_some_and(|p| path_matches(p, filters))
        })
        .collect()
}

/// Look up the full manifest at `head`, if any.
fn head_manifest_for(
    repo: &dyn Repository,
    head: &Option<Hash>,
) -> Result<Option<oak_core::Manifest>> {
    Ok(if let Some(head_hash) = head {
        repo.get_commit(head_hash)?
            .and_then(|c| repo.get_manifest(&c.manifest_hash).ok().flatten())
    } else {
        None
    })
}

/// Index a manifest's entries by path for pre-image lookups.
fn manifest_file_map(manifest: &Option<oak_core::Manifest>) -> HashMap<&str, &ManifestEntry> {
    manifest
        .as_ref()
        .map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default()
}

/// Render the `--stat` summary: one `<kind> <path>  +added -removed` row per
/// change and a totals line. Counts are derived from the same old/new bytes and
/// text-diff rules as the full diff, without building unified diff text first
/// (binary files show `bin` instead of counts).
pub(crate) fn render_stat(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
) -> Result<Vec<String>> {
    render_stat_with_worktree_blobs(repo, work_tree, head_files, changes, &HashMap::new())
}

fn render_stat_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<String>> {
    render_stat_with_limit(repo, work_tree, head_files, changes, None, worktree_blobs)
}

pub(crate) fn render_stat_with_limit(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    max_rows: Option<usize>,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<String>> {
    let mut rows = Vec::with_capacity(changes.len() + 1);
    let (mut total_added, mut total_removed) = (0usize, 0usize);
    for (index, change) in changes.iter().enumerate() {
        let stat = stat_counts_for_change(repo, work_tree, head_files, change, worktree_blobs)?;
        let kind = match change.change_type {
            ChangeType::Added => "A",
            ChangeType::Modified => "M",
            ChangeType::Deleted => "D",
            ChangeType::Renamed => "R",
        };
        let label = match (change.change_type, &change.old_path) {
            (ChangeType::Renamed, Some(old)) => format!("{old} -> {}", change.path),
            _ => change.path.clone(),
        };
        let show_row = max_rows.is_none_or(|limit| index < limit);
        if stat.binary {
            if show_row {
                rows.push(format!("{kind} {label}  bin"));
            }
        } else {
            if show_row {
                rows.push(format!("{kind} {label}  +{} -{}", stat.added, stat.removed));
            }
            total_added += stat.added;
            total_removed += stat.removed;
        }
    }
    if let Some(limit) = max_rows {
        if changes.len() > limit {
            rows.push(format!("... {} more", changes.len() - limit));
        }
    }
    rows.push(format!(
        "{} file{} changed, +{total_added} -{total_removed}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
    ));
    Ok(rows)
}

/// Emit an already-formatted (colorized) diff string: through `$PAGER` when
/// stdout is a terminal, otherwise straight to stdout line by line.
///
/// Shared by the regular `oak diff` and the mount-aware `oak diff` so both
/// paginate and print identically.
pub(crate) fn print_formatted(formatted: &str) {
    // If stdout is a terminal, pipe through a pager
    if io::stdout().is_terminal() {
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
        let mut args: Vec<&str> = pager.split_whitespace().collect();
        let cmd = args.remove(0);

        // Add -R for less to support ANSI colors, -F to quit if output fits on screen
        if cmd == "less" && args.is_empty() {
            args.push("-RF");
        }

        if let Ok(mut child) = Command::new(cmd).args(&args).stdin(Stdio::piped()).spawn() {
            if let Some(ref mut stdin) = child.stdin {
                // Ignore broken pipe (user quit pager early)
                let _ = stdin.write_all(formatted.as_bytes());
            }
            let _ = child.wait();
            return;
        }
        // Fall through to direct output if pager fails to spawn
    }

    // Non-TTY or pager failed: print directly
    for line in formatted.lines() {
        output::print_line(line);
    }
}

fn print_changes(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<()> {
    if io::stdout().is_terminal() {
        let activity = output::activity("Rendering text diffs...");
        let mut formatted = String::new();
        for change in changes {
            formatted.push_str(&render_change_with_worktree_blobs(
                repo,
                work_tree,
                head_files,
                change,
                worktree_blobs,
            )?);
            formatted.push('\n');
        }
        activity.finish_and_clear();
        print_formatted(&formatted);
        return Ok(());
    }

    for change in changes {
        let block =
            render_change_with_worktree_blobs(repo, work_tree, head_files, change, worktree_blobs)?;
        for line in block.lines() {
            output::print_line(line);
        }
        output::print_line("");
    }
    Ok(())
}

/// Compute the change set and render it to a formatted (colorized) diff string.
///
/// Shared by [`run`] and exercised directly by tests. The returned `changes`
/// are exactly what [`super::commit::compute_changes_for_status`] (and therefore
/// `oak status`) report, and `formatted` is guaranteed to contain a
/// `diff --oak a/<path> b/<path>` block for *every* one of them — diff never
/// drops a path in the set, so `oak status`, `oak diff`, and `oak commit` agree.
pub fn render(repo: &dyn Repository, work_tree: &Path) -> Result<(Vec<FileChange>, String)> {
    // Authoritative change set — identical to `oak status` and to what
    // `oak commit` will record (honors the active project/team scope, the
    // parent-branch head walk, and counts by blob hash rather than by whether a
    // textual diff happens to render).
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo, work_tree)?;

    // Full head manifest, for looking up the pre-image blob of each change.
    // (The change set already restricts which paths we render.)
    let head_manifest = if let Some(ref head_hash) = head {
        repo.get_commit(head_hash)?
            .and_then(|c| repo.get_manifest(&c.manifest_hash).ok().flatten())
    } else {
        None
    };
    let head_files: HashMap<&str, &ManifestEntry> = head_manifest
        .as_ref()
        .map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();

    let mut formatted = String::new();
    for change in &changes {
        formatted.push_str(&render_change_with_worktree_blobs(
            repo,
            work_tree,
            &head_files,
            change,
            &worktree_blobs,
        )?);
        formatted.push('\n');
    }
    Ok((changes, formatted))
}

/// Render one change into a formatted (colorized) diff block. Always produces
/// output — a file in the change set is never dropped, even when there's no
/// textual hunk to show.
///
/// This is the colorized wrapper over [`render_change_lines`]: it takes the raw
/// per-line block that function produces and runs each line through
/// [`output::format_diff_line`]. So `oak diff --print` and the interactive
/// browser render exactly the same lines, just one with ANSI codes and one with
/// ratatui styles.
pub(crate) fn render_change(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
) -> Result<String> {
    render_change_with_worktree_blobs(repo, work_tree, head_files, change, &HashMap::new())
}

fn render_change_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<String> {
    let mut block = String::new();
    for line in render_change_lines_with_worktree_blobs(
        repo,
        work_tree,
        head_files,
        change,
        worktree_blobs,
    )? {
        block.push_str(&output::format_diff_line(&line));
        block.push('\n');
    }
    Ok(block)
}

struct StatCounts {
    added: usize,
    removed: usize,
    binary: bool,
}

fn stat_counts_for_change(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<StatCounts> {
    let (old_bytes, new_bytes) = change_bytes(repo, work_tree, head_files, change, worktree_blobs)?;
    if oak_core::binary_or_large_notice(&change.path, &old_bytes, &new_bytes).is_some() {
        return Ok(StatCounts {
            added: 0,
            removed: 0,
            binary: true,
        });
    }

    let old_str = String::from_utf8_lossy(&old_bytes);
    let new_str = String::from_utf8_lossy(&new_bytes);
    let diff = FileDiff::new(&change.path, &old_str, &new_str);
    let mut added = 0;
    let mut removed = 0;
    for line in &diff.lines {
        match line {
            DiffLine::Added(_) => added += 1,
            DiffLine::Removed(_) => removed += 1,
            DiffLine::Context(_) => {}
        }
    }

    Ok(StatCounts {
        added,
        removed,
        binary: false,
    })
}

/// `(old_bytes, new_bytes)` for a changed file — borrowed from the worktree
/// blob cache where possible, owned otherwise.
type DiffBytes<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);

fn change_bytes<'a>(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &'a HashMap<Hash, Vec<u8>>,
) -> Result<DiffBytes<'a>> {
    let path = &change.path;
    let current_bytes = || -> Result<Cow<'a, [u8]>> {
        if let Some(hash) = &change.new_blob_hash {
            if let Some(bytes) = worktree_blobs.get(hash) {
                return Ok(Cow::Borrowed(bytes.as_slice()));
            }
        }
        Ok(Cow::Owned(read_worktree_bytes(work_tree, path)?))
    };
    match change.change_type {
        ChangeType::Added => Ok((Cow::Borrowed(&[]), current_bytes()?)),
        ChangeType::Deleted => Ok((
            Cow::Owned(old_bytes_for(
                repo,
                head_files,
                path,
                &change.old_blob_hash,
            )?),
            Cow::Borrowed(&[]),
        )),
        ChangeType::Modified => Ok((
            Cow::Owned(old_bytes_for(
                repo,
                head_files,
                path,
                &change.old_blob_hash,
            )?),
            current_bytes()?,
        )),
        ChangeType::Renamed => {
            let old_path = change.old_path.as_deref().unwrap_or(path);
            Ok((
                Cow::Owned(old_bytes_for(
                    repo,
                    head_files,
                    old_path,
                    &change.old_blob_hash,
                )?),
                current_bytes()?,
            ))
        }
    }
}

fn old_bytes_for(
    repo: &dyn Repository,
    head_files: &HashMap<&str, &ManifestEntry>,
    path: &str,
    recorded: &Option<Hash>,
) -> Result<Vec<u8>> {
    if let Some(hash) = recorded {
        read_blob_bytes(repo, hash)
    } else if let Some(entry) = head_files.get(path) {
        read_blob_bytes(repo, &entry.blob_hash)
    } else {
        Ok(Vec::new())
    }
}

fn read_blob_bytes(repo: &dyn Repository, hash: &Hash) -> Result<Vec<u8>> {
    Ok(repo.get_blob(hash)?.map(|b| b.content).unwrap_or_default())
}

fn read_worktree_bytes(work_tree: &Path, rel: &str) -> Result<Vec<u8>> {
    let path = work_tree.join(rel);
    if path.exists() {
        Ok(fs::read(path)?)
    } else {
        Ok(Vec::new())
    }
}

/// Compute the raw (uncolored) diff block lines for one change — the single
/// source of truth for *what* a change's diff looks like. The first line is
/// always the `diff --oak a/<path> b/<path>` header so the path is never
/// dropped, even when there's no textual hunk (binary, pure rename, mode-only,
/// trailing-newline-only). [`render_change`] colorizes these for `--print`; the
/// TUI in [`build_entries`] styles them per-line for the browser.
pub(crate) fn render_change_lines(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
) -> Result<Vec<String>> {
    render_change_lines_with_worktree_blobs(repo, work_tree, head_files, change, &HashMap::new())
}

fn render_change_lines_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<String>> {
    let path = &change.path;
    let (old_bytes, new_bytes) = change_bytes(repo, work_tree, head_files, change, worktree_blobs)?;
    let rename_header = match change.change_type {
        ChangeType::Renamed => {
            let old_path = change.old_path.as_deref().unwrap_or(path);
            Some(format!("rename from {old_path}\nrename to {path}"))
        }
        _ => None,
    };

    // Mode change (e.g. chmod +x), read straight off the change.
    let mode_header = match (change.old_mode, change.new_mode) {
        (Some(o), Some(n)) if o != n => Some(format!(
            "old mode {}\nnew mode {}",
            mode_octal(o),
            mode_octal(n)
        )),
        _ => None,
    };

    let mut lines: Vec<String> = Vec::new();

    // Binary or very large content can't be shown usefully as an inline text
    // diff. Emit a notice so the path still appears — status and commit count
    // it, so diff must too.
    if let Some(notice) = oak_core::binary_or_large_notice(path, &old_bytes, &new_bytes) {
        lines.push(format!("diff --oak a/{path} b/{path}"));
        if let Some(h) = &rename_header {
            lines.extend(h.lines().map(str::to_string));
        }
        if let Some(h) = &mode_header {
            lines.extend(h.lines().map(str::to_string));
        }
        lines.push(notice);
        return Ok(lines);
    }

    let old_str = String::from_utf8_lossy(&old_bytes);
    let new_str = String::from_utf8_lossy(&new_bytes);
    let diff = FileDiff::new(path, &old_str, &new_str);
    let unified = diff.to_unified();

    if !unified.is_empty() {
        // Insert the rename/mode header right after the `diff --oak` line.
        let header = rename_header.as_deref().or(mode_header.as_deref());
        for line in unified.lines() {
            lines.push(line.to_string());
            if line.starts_with("diff --oak") {
                if let Some(h) = header {
                    lines.extend(h.lines().map(str::to_string));
                }
            }
        }
    } else {
        // No textual hunk: a pure rename, a mode-only change, or a byte-level
        // change the line differ can't represent (e.g. a trailing-newline
        // difference). The blob hash still differs, so emit the header — never
        // drop the path.
        lines.push(format!("diff --oak a/{path} b/{path}"));
        if let Some(h) = &rename_header {
            lines.extend(h.lines().map(str::to_string));
        } else if let Some(h) = &mode_header {
            lines.extend(h.lines().map(str::to_string));
        } else {
            lines.push(format!("Files a/{path} and b/{path} differ"));
        }
    }
    Ok(lines)
}

fn mode_octal(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
    }
}

// ---------------------------------------------------------------------------
// Interactive file-tree browser
//
// `oak diff` (no `--print`, attached to a terminal) opens a two-pane TUI: a
// collapsible tree of the changed files on the left, the selected file's diff
// on the right. The diff lines come from the very same [`render_change_lines`]
// that `--print` colorizes, so the two views never disagree about content.
// ---------------------------------------------------------------------------

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

/// One changed file, ready for the browser: its path, how to label it (renames
/// show `old -> new`), the kind of change, and the raw diff block lines.
pub(crate) struct FileEntry {
    path: String,
    display_path: String,
    change_type: ChangeType,
    lines: Vec<String>,
}

/// Build the per-file entries the browser navigates from an already-computed
/// change set. Shared by the working-tree diff ([`run_tui`]) and the mount diff
/// so both browse identically.
pub(crate) fn build_entries(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
) -> Result<Vec<FileEntry>> {
    build_entries_with_worktree_blobs(repo, work_tree, head_files, changes, &HashMap::new())
}

fn build_entries_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::with_capacity(changes.len());
    for change in changes {
        let lines = if worktree_blobs.is_empty() {
            render_change_lines(repo, work_tree, head_files, change)?
        } else {
            render_change_lines_with_worktree_blobs(
                repo,
                work_tree,
                head_files,
                change,
                worktree_blobs,
            )?
        };
        let display_path = match (change.change_type, &change.old_path) {
            (ChangeType::Renamed, Some(old)) => format!("{old} -> {}", change.path),
            _ => change.path.clone(),
        };
        entries.push(FileEntry {
            path: change.path.clone(),
            display_path,
            change_type: change.change_type,
            lines,
        });
    }
    Ok(entries)
}

/// Launch the interactive file-tree diff browser for the working tree vs HEAD.
///
/// Falls back to the stat summary when stdout is not a terminal. Agents and
/// scripts usually need path/count signal first, not full hunks; callers that
/// need patches can still request them explicitly with `oak diff --print`.
pub fn run_tui(path: &Path, paths: &[std::path::PathBuf]) -> Result<()> {
    if !io::stdout().is_terminal() {
        return run_stat_with_row_limit(path, paths, None);
    }

    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    // Same authoritative change set + pre-image lookup `render` builds,
    // restricted to the user's paths exactly like the printed diff.
    let filters = resolve_filters(path, &work_tree, paths)?;
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo.as_ref(), &work_tree)?;
    // Emitted before the alt-screen TUI takes over; the terminal shows it
    // again once the browser exits.
    super::commit::warn_tracked_now_ignored(
        &changes,
        &work_tree,
        "will be removed from the branch by the next commit",
    );
    let changes = filter_changes(changes, &filters);
    if changes.is_empty() {
        output::info(no_differences_message(&filters));
        return Ok(());
    }
    let head_manifest = head_manifest_for(repo.as_ref(), &head)?;
    let head_files = manifest_file_map(&head_manifest);

    let entries = build_entries_with_worktree_blobs(
        repo.as_ref(),
        &work_tree,
        &head_files,
        &changes,
        &worktree_blobs,
    )?;
    run_tui_entries(entries, "working tree")
}

fn run_stat_with_row_limit(
    path: &Path,
    paths: &[std::path::PathBuf],
    max_rows: Option<usize>,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    let filters = resolve_filters(path, &work_tree, paths)?;
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo.as_ref(), &work_tree)?;
    super::commit::warn_tracked_now_ignored(
        &changes,
        &work_tree,
        "will be removed from the branch by the next commit",
    );
    let changes = filter_changes(changes, &filters);
    if changes.is_empty() {
        output::info(no_differences_message(&filters));
        return Ok(());
    }

    let head_manifest = head_manifest_for(repo.as_ref(), &head)?;
    let head_files = manifest_file_map(&head_manifest);
    for line in render_stat_with_limit(
        repo.as_ref(),
        &work_tree,
        &head_files,
        &changes,
        max_rows,
        &worktree_blobs,
    )? {
        output::print_line(&line);
    }
    Ok(())
}

/// A node in the changed-files tree: either a directory (with children) or a
/// leaf file (carrying its index into the entries vec).
struct Node {
    name: String,
    is_dir: bool,
    expanded: bool,
    file_idx: Option<usize>,
    children: Vec<Node>,
}

fn insert_path(nodes: &mut Vec<Node>, parts: &[&str], idx: usize) {
    let (head, rest) = match parts.split_first() {
        Some(v) => v,
        None => return,
    };
    if rest.is_empty() {
        nodes.push(Node {
            name: (*head).to_string(),
            is_dir: false,
            expanded: false,
            file_idx: Some(idx),
            children: Vec::new(),
        });
        return;
    }
    if let Some(pos) = nodes.iter().position(|n| n.is_dir && n.name == *head) {
        insert_path(&mut nodes[pos].children, rest, idx);
    } else {
        let mut dir = Node {
            name: (*head).to_string(),
            is_dir: true,
            expanded: true,
            file_idx: None,
            children: Vec::new(),
        };
        insert_path(&mut dir.children, rest, idx);
        nodes.push(dir);
    }
}

/// Sort each level: directories first, then files, alphabetically within each.
fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    for n in nodes.iter_mut() {
        if n.is_dir {
            sort_nodes(&mut n.children);
        }
    }
}

fn build_tree(entries: &[FileEntry]) -> Vec<Node> {
    let mut roots = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let parts: Vec<&str> = e.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        insert_path(&mut roots, &parts, i);
    }
    sort_nodes(&mut roots);
    roots
}

/// A flattened, currently-visible tree row (collapsed subtrees omitted).
struct Row {
    depth: usize,
    idx_path: Vec<usize>,
    is_dir: bool,
    expanded: bool,
    name: String,
    file_idx: Option<usize>,
}

fn flatten(nodes: &[Node], depth: usize, prefix: &mut Vec<usize>, out: &mut Vec<Row>) {
    for (i, n) in nodes.iter().enumerate() {
        prefix.push(i);
        out.push(Row {
            depth,
            idx_path: prefix.clone(),
            is_dir: n.is_dir,
            expanded: n.expanded,
            name: n.name.clone(),
            file_idx: n.file_idx,
        });
        if n.is_dir && n.expanded {
            flatten(&n.children, depth + 1, prefix, out);
        }
        prefix.pop();
    }
}

fn node_at_mut<'a>(nodes: &'a mut [Node], idx_path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = idx_path.split_first()?;
    let node = nodes.get_mut(*first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        node_at_mut(&mut node.children, rest)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Diff,
}

struct App {
    entries: Vec<FileEntry>,
    roots: Vec<Node>,
    rows: Vec<Row>,
    selected: usize,
    focus: Focus,
    cur_file: Option<usize>,
    diff_scroll: usize,
    title: String,
    quit: bool,
}

impl App {
    fn new(entries: Vec<FileEntry>, title: String) -> Self {
        let roots = build_tree(&entries);
        let mut app = App {
            entries,
            roots,
            rows: Vec::new(),
            selected: 0,
            focus: Focus::Tree,
            cur_file: None,
            diff_scroll: 0,
            title,
            quit: false,
        };
        app.rebuild_rows();
        // Start on the first actual file, not a parent directory.
        if let Some(pos) = app.rows.iter().position(|r| r.file_idx.is_some()) {
            app.selected = pos;
        }
        app.sync_current();
        app
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        flatten(&self.roots, 0, &mut Vec::new(), &mut rows);
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// Keep `cur_file`/`diff_scroll` in step with the selected row: when the
    /// selection lands on a different file, show its diff from the top.
    fn sync_current(&mut self) {
        let new = self.rows.get(self.selected).and_then(|r| r.file_idx);
        if new != self.cur_file {
            self.cur_file = new;
            self.diff_scroll = 0;
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.selected as i32;
        let next = (cur + delta).clamp(0, self.rows.len() as i32 - 1) as usize;
        self.selected = next;
        self.sync_current();
    }

    fn select_row(&mut self, n: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = n.min(self.rows.len() - 1);
        self.sync_current();
    }

    fn toggle_dir(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let idx_path = row.idx_path.clone();
        if let Some(node) = node_at_mut(&mut self.roots, &idx_path) {
            node.expanded = !node.expanded;
        }
        self.rebuild_rows();
        self.sync_current();
    }

    /// Enter / l: expand a collapsed dir, step into an expanded one, or jump to
    /// the diff pane on a file.
    fn expand_or_focus(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.file_idx.is_some() {
            if self.cur_file.is_some() {
                self.focus = Focus::Diff;
            }
            return;
        }
        if row.is_dir {
            if row.expanded {
                self.move_sel(1);
            } else {
                self.toggle_dir();
            }
        }
    }

    /// h / ←: collapse an expanded dir.
    fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir && row.expanded {
            self.toggle_dir();
        }
    }

    fn activate(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir {
            self.toggle_dir();
        } else if self.cur_file.is_some() {
            self.focus = Focus::Diff;
        }
    }

    fn scroll_diff(&mut self, delta: i32) {
        let next = self.diff_scroll as i32 + delta;
        self.diff_scroll = next.max(0) as usize; // upper bound clamped at draw
    }
}

/// RAII restore of terminal state — see the twin in `log.rs` for why `Show` is
/// load-bearing. Raw mode + alternate screen on `enter`, undone on `drop` even
/// through `?` early-returns and panics.
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
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(Show);
        let _ = disable_raw_mode();
    }
}

/// Run the browser over a prepared set of file entries. Returns immediately with
/// a "No differences" notice if the set is empty.
pub(crate) fn run_tui_entries(entries: Vec<FileEntry>, title: &str) -> Result<()> {
    if entries.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(entries, title.to_string());

    while !app.quit {
        terminal.draw(|f| draw(f, &mut app))?;
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key);
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, ev: KeyEvent) {
    let key = ev.code;
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    match app.focus {
        Focus::Tree => match key {
            KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
            KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
            KeyCode::Char('b') if ctrl => app.move_sel(-10),
            KeyCode::Char('f') if ctrl => app.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => app.select_row(0),
            KeyCode::End | KeyCode::Char('G') => app.select_row(usize::MAX),
            KeyCode::Enter => app.activate(),
            KeyCode::Char(' ') => app.toggle_dir(),
            KeyCode::Right | KeyCode::Char('l') => app.expand_or_focus(),
            KeyCode::Left | KeyCode::Char('h') => app.collapse(),
            KeyCode::Tab => {
                if app.cur_file.is_some() {
                    app.focus = Focus::Diff;
                }
            }
            KeyCode::PageUp => app.scroll_diff(-20),
            KeyCode::PageDown => app.scroll_diff(20),
            _ => {}
        },
        Focus::Diff => match key {
            KeyCode::Char('q')
            | KeyCode::Esc
            | KeyCode::Left
            | KeyCode::Char('h')
            | KeyCode::Tab => app.focus = Focus::Tree,
            KeyCode::Up | KeyCode::Char('k') => app.scroll_diff(-1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_diff(1),
            KeyCode::PageUp => app.scroll_diff(-20),
            KeyCode::PageDown => app.scroll_diff(20),
            KeyCode::Char('b') if ctrl => app.scroll_diff(-20),
            KeyCode::Char('f') if ctrl => app.scroll_diff(20),
            KeyCode::Char('u') if ctrl => app.scroll_diff(-10),
            KeyCode::Char('d') if ctrl => app.scroll_diff(10),
            KeyCode::Home | KeyCode::Char('g') => app.diff_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => app.diff_scroll = usize::MAX,
            _ => {}
        },
    }
}

fn status_glyph(ct: ChangeType) -> (&'static str, Color) {
    match ct {
        ChangeType::Added => ("A", Color::Green),
        ChangeType::Modified => ("M", Color::Yellow),
        ChangeType::Deleted => ("D", Color::Red),
        ChangeType::Renamed => ("R", Color::Cyan),
    }
}

/// Style a raw diff line for the browser — same prefix heuristic as the ANSI
/// [`output::format_diff_line`] used by `--print`.
fn style_diff_line(line: &str) -> Style {
    if line.starts_with('+') && !line.starts_with("+++") {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with("diff ") || line.starts_with("--- ") || line.starts_with("+++ ") {
        Style::default().bold()
    } else {
        Style::default()
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(20)])
        .split(outer[0]);

    draw_tree(f, app, panes[0]);
    draw_diff(f, app, panes[1]);

    let help = if app.focus == Focus::Tree {
        " ↑↓/jk move · ←/→ collapse/expand · Enter/Tab view diff · g/G top/bottom · q quit"
    } else {
        " ↑↓/jk scroll · PgUp/PgDn/^F/^B page · ^U/^D half · g/G top/bottom · ←/Esc/Tab back to tree"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        outer[1],
    );
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let tree_focused = app.focus == Focus::Tree;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            if row.is_dir {
                let arrow = if row.expanded { "▾" } else { "▸" };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{indent}{arrow} {}/", row.name),
                    Style::default().fg(Color::Blue).bold(),
                )]))
            } else {
                let (letter, color) = row
                    .file_idx
                    .map(|i| status_glyph(app.entries[i].change_type))
                    .unwrap_or((" ", Color::Gray));
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(format!("{letter} "), Style::default().fg(color)),
                    Span::raw(row.name.clone()),
                ]))
            }
        })
        .collect();

    let n = app.entries.len();
    let title = format!(
        " {} · {} file{} ",
        app.title,
        n,
        if n == 1 { "" } else { "s" }
    );
    let border_style = if tree_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let highlight = if tree_focused {
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
        .highlight_symbol("");

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let diff_focused = app.focus == Focus::Diff;
    let border_style = if diff_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let (title, lines): (String, Vec<Line>) = match app.cur_file {
        Some(i) => {
            let content_height = area.height.saturating_sub(2) as usize;
            let max_scroll = app.entries[i].lines.len().saturating_sub(content_height);
            // Clamp here so PageDown / G can overshoot in the handler.
            if app.diff_scroll > max_scroll {
                app.diff_scroll = max_scroll;
            }
            let scroll = app.diff_scroll;
            let entry = &app.entries[i];
            let lines: Vec<Line> = entry
                .lines
                .iter()
                .skip(scroll)
                .take(content_height)
                .map(|l| Line::from(Span::styled(l.clone(), style_diff_line(l))))
                .collect();
            (format!(" {} ", entry.display_path), lines)
        }
        None => (
            " diff ".to_string(),
            vec![Line::from(Span::styled(
                "Select a file to view its diff",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::SqliteRepository;
    use tempfile::TempDir;

    fn change(path: &str, old_path: Option<&str>) -> FileChange {
        FileChange {
            path: path.to_string(),
            change_type: if old_path.is_some() {
                ChangeType::Renamed
            } else {
                ChangeType::Modified
            },
            old_path: old_path.map(str::to_string),
            old_blob_hash: None,
            new_blob_hash: None,
            old_mode: None,
            new_mode: None,
        }
    }

    fn paths(changes: &[FileChange]) -> Vec<&str> {
        changes.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn filter_matches_directories_component_wise_not_by_string_prefix() {
        let changes = vec![
            change("src/a.txt", None),
            change("src2/b.txt", None),
            change("src/nested/c.txt", None),
        ];
        // `src` must match files *under* src/, never src2/.
        let filtered = filter_changes(changes, &["src".to_string()]);
        assert_eq!(paths(&filtered), vec!["src/a.txt", "src/nested/c.txt"]);
    }

    #[test]
    fn filter_matches_exact_files_and_trailing_slash_dirs() {
        let changes = vec![change("src/a.txt", None), change("docs/x.md", None)];
        let filtered = filter_changes(changes.clone(), &["docs/x.md".to_string()]);
        assert_eq!(paths(&filtered), vec!["docs/x.md"]);
        // A trailing slash on a directory filter is accepted.
        let filtered = filter_changes(changes, &["src/".to_string()]);
        assert_eq!(paths(&filtered), vec!["src/a.txt"]);
    }

    #[test]
    fn filter_matches_renames_on_either_side() {
        let changes = vec![change("new/dest.txt", Some("old/source.txt"))];
        // Filtering by where the file *was* still surfaces the rename.
        let filtered = filter_changes(changes.clone(), &["old".to_string()]);
        assert_eq!(paths(&filtered), vec!["new/dest.txt"]);
        let filtered = filter_changes(changes, &["new".to_string()]);
        assert_eq!(paths(&filtered), vec!["new/dest.txt"]);
    }

    #[test]
    fn empty_filter_set_passes_everything_and_root_filter_matches_all() {
        let changes = vec![change("a.txt", None), change("b/c.txt", None)];
        assert_eq!(filter_changes(changes.clone(), &[]).len(), 2);
        // `oak diff .` from the repo root resolves to the empty repo-relative
        // path, which matches everything.
        assert_eq!(filter_changes(changes, &[String::new()]).len(), 2);
    }

    #[test]
    fn render_change_uses_ephemeral_worktree_bytes_without_rereading_disk() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let new_bytes = b"line one\nline two\n".to_vec();
        let new_hash = oak_core::hash_bytes(&new_bytes);
        let change = FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Added,
            old_path: None,
            old_blob_hash: None,
            new_blob_hash: Some(new_hash.clone()),
            old_mode: None,
            new_mode: None,
        };
        let mut worktree_blobs = HashMap::new();
        worktree_blobs.insert(new_hash, new_bytes);

        let lines = render_change_lines_with_worktree_blobs(
            &repo,
            temp.path(),
            &HashMap::new(),
            &change,
            &worktree_blobs,
        )
        .unwrap();

        assert!(
            lines.iter().any(|line| line == "+line two"),
            "diff should reuse the scan's worktree bytes instead of rereading a missing file: {lines:?}"
        );
    }
}
