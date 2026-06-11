#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use oak_core::{Branch, BranchStatus, ChangeType, Commit, FileChange, FileStatus};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Verbose / timing instrumentation
// ---------------------------------------------------------------------------

static VERBOSE: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

/// Enable verbose timing output. Picks up the `OAK_VERBOSE=1` env var as well.
pub fn enable_verbose() {
    VERBOSE.store(true, Ordering::Relaxed);
    START.get_or_init(Instant::now);
}

/// Returns true if verbose output is enabled (via flag or `OAK_VERBOSE`).
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Emit a timestamped verbose line to stderr (no-op if verbose is off).
/// Format: `[+1.234s] message` with the elapsed wall time since `enable_verbose()`.
pub fn vlog(msg: &str) {
    if !is_verbose() {
        return;
    }
    let elapsed = START.get_or_init(Instant::now).elapsed();
    eprintln!(
        "{}[+{:>7.3}s]{} {}",
        colors::DIM.for_stderr(),
        elapsed.as_secs_f64(),
        colors::RESET.for_stderr(),
        msg
    );
}

// ---------------------------------------------------------------------------
// Thread-local output capture (used by oak-mcp to call CLI functions directly)
// ---------------------------------------------------------------------------

thread_local! {
    static CAPTURE_BUFFER: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Start capturing output to an in-memory buffer instead of stdout.
pub fn begin_capture() {
    CAPTURE_BUFFER.with(|buf| {
        *buf.borrow_mut() = Some(Vec::new());
    });
}

/// Stop capturing and return all lines joined by newlines.
pub fn end_capture() -> String {
    CAPTURE_BUFFER.with(|buf| buf.borrow_mut().take().unwrap_or_default().join("\n"))
}

/// Internal: write a complete line to the capture buffer or to stdout.
///
/// Lines arrive here already colored or already plain — the `colors`
/// constants decide at render time (see the color-gating section below) —
/// so this writes them verbatim.
fn emit_line(s: &str) {
    let captured = CAPTURE_BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        if let Some(ref mut lines) = *buf {
            lines.push(s.to_string());
            true
        } else {
            false
        }
    });
    if !captured {
        println!("{s}");
    }
}

/// Internal: write a complete diagnostic line to the capture buffer or to
/// stderr. Warnings and errors go here rather than [`emit_line`] so they
/// never interleave with parseable data on stdout — piping `oak log`/`oak
/// diff` yields data only, while diagnostics stay visible on the terminal.
fn emit_stderr_line(s: &str) {
    let captured = CAPTURE_BUFFER.with(|buf| {
        let mut buf = buf.borrow_mut();
        if let Some(ref mut lines) = *buf {
            lines.push(s.to_string());
            true
        } else {
            false
        }
    });
    if !captured {
        eprintln!("{s}");
    }
}

/// Print a plain line (replaces direct `println!` calls in command modules).
pub fn print_line(msg: &str) {
    emit_line(msg);
}

/// Print a single compact JSON document to stdout.
pub fn print_json<T: Serialize>(value: &T) -> oak_core::Result<()> {
    emit_line(&serde_json::to_string(value)?);
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct StatusJson {
    pub branch: Option<String>,
    pub branch_description: Option<String>,
    pub parent: Option<String>,
    pub head: Option<String>,
    pub branch_status: Option<String>,
    pub unmerged_commit_count: usize,
    pub changes: Vec<StatusChangeJson>,
    pub merge_in_progress: bool,
    pub sync_in_progress: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusChangeJson {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LogCommitJson {
    pub hash: String,
    pub timestamp: String,
    pub branch: String,
    pub description_or_subject: String,
    pub files_changed: usize,
}

#[derive(Debug, Serialize)]
pub struct BranchJson {
    pub name: String,
    pub head: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub created_at: String,
    pub current: bool,
}

// ---------------------------------------------------------------------------
// Color gating: never send ANSI escapes to a non-terminal stream
// ---------------------------------------------------------------------------
//
// The decision is made *before* any output is formatted: each `colors`
// constant renders as its escape code only when the destination stream wants
// color, and as nothing otherwise. Gating at render time (rather than
// stripping escapes from the final string) matters for correctness — by the
// time a line is assembled it contains user-controlled content (file
// contents in diffs, paths, branch descriptions), and a post-hoc strip would
// silently mangle data that legitimately contains literal ESC bytes.

/// The color decision for one stream, from the de facto standard env vars:
/// `NO_COLOR` (any non-empty value disables, <https://no-color.org>) wins over
/// `CLICOLOR_FORCE` (non-empty and not "0" forces color even when piped),
/// which wins over plain TTY detection.
fn should_color(no_color: Option<&OsStr>, force: Option<&OsStr>, is_tty: bool) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if force.is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    is_tty
}

/// Whether ANSI colors should be written to stdout. Computed once per process.
pub fn stdout_colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        should_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("CLICOLOR_FORCE").as_deref(),
            std::io::stdout().is_terminal(),
        )
    })
}

/// Whether ANSI colors should be written to stderr. Computed once per process.
pub fn stderr_colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        should_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("CLICOLOR_FORCE").as_deref(),
            std::io::stderr().is_terminal(),
        )
    })
}

/// ANSI color codes, gated at render time.
///
/// A [`Color`] used in a format string (the overwhelmingly common case —
/// everything destined for stdout) displays as its escape code only when
/// [`stdout_colors_enabled`]. Either way, a disabled color contributes zero
/// bytes — nothing is ever stripped after formatting.
///
/// **Writing to stderr?** `Display` gates on *stdout*'s decision, which can
/// differ from stderr's (e.g. `oak status | cat` leaves stderr a TTY). Any
/// `eprintln!` must use [`Color::for_stderr`] instead of `{}` formatting —
/// see `vlog` and the upgrade notice in `version_check` for the pattern.
pub mod colors {
    use std::fmt;

    /// One ANSI SGR escape code that knows when to stay silent.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct Color(&'static str);

    impl Color {
        /// The escape code if `enabled`, the empty string otherwise.
        pub fn when(self, enabled: bool) -> &'static str {
            if enabled {
                self.0
            } else {
                ""
            }
        }

        /// The escape code gated on stderr's color decision — for the few
        /// writers that print to stderr (verbose timing, upgrade notice).
        pub fn for_stderr(self) -> &'static str {
            self.when(super::stderr_colors_enabled())
        }
    }

    impl fmt::Display for Color {
        /// Render for **stdout**: the escape code when stdout wants color,
        /// nothing otherwise. Do not rely on this in strings destined for
        /// stderr — use [`Color::for_stderr`] there, since the two streams'
        /// color decisions can differ.
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.when(super::stdout_colors_enabled()))
        }
    }

    pub const RESET: Color = Color("\x1b[0m");
    pub const RED: Color = Color("\x1b[31m");
    pub const GREEN: Color = Color("\x1b[32m");
    pub const YELLOW: Color = Color("\x1b[33m");
    pub const BLUE: Color = Color("\x1b[34m");
    pub const MAGENTA: Color = Color("\x1b[35m");
    pub const CYAN: Color = Color("\x1b[36m");
    pub const WHITE: Color = Color("\x1b[37m");
    pub const BRIGHT_GREEN: Color = Color("\x1b[92m");
    pub const BOLD: Color = Color("\x1b[1m");
    pub const DIM: Color = Color("\x1b[2m");
    pub const UNDERLINE: Color = Color("\x1b[4m");
}

/// Print a success message
pub fn success(msg: &str) {
    emit_line(&format!(
        "{}{}✓{} {}",
        colors::GREEN,
        colors::BOLD,
        colors::RESET,
        msg
    ));
}

/// Print an info message
pub fn info(msg: &str) {
    emit_line(msg);
}

/// Print a warning message (to stderr — diagnostics stay off the data channel)
pub fn warning(msg: &str) {
    emit_stderr_line(&format!(
        "{}{}warning:{} {}",
        colors::YELLOW.for_stderr(),
        colors::BOLD.for_stderr(),
        colors::RESET.for_stderr(),
        msg
    ));
}

/// Print an error message (to stderr — diagnostics stay off the data channel)
pub fn error(msg: &str) {
    emit_stderr_line(&format!(
        "{}{}error:{} {}",
        colors::RED.for_stderr(),
        colors::BOLD.for_stderr(),
        colors::RESET.for_stderr(),
        msg
    ));
}

/// Print a section header (bold + cyan)
pub fn header(msg: &str) {
    emit_line(&format!(
        "{}{}{}{}",
        colors::CYAN,
        colors::BOLD,
        msg,
        colors::RESET,
    ));
}

/// Print an indented detail line (dimmed prefix)
pub fn detail(label: &str, value: &str) {
    emit_line(&format!(
        "  {}{}{} {}",
        colors::DIM,
        label,
        colors::RESET,
        value,
    ));
}

/// Print an indented item (used for file lists, commit lists, etc.)
pub fn item(msg: &str) {
    emit_line(&format!("  {msg}"));
}

/// Print a blank line separator
pub fn blank() {
    emit_line("");
}

/// Format a file status for display
pub fn format_status(status: FileStatus, path: &str) -> String {
    let (color, prefix) = match status {
        FileStatus::Added => (colors::GREEN, "A"),
        FileStatus::Modified => (colors::YELLOW, "M"),
        FileStatus::Deleted => (colors::RED, "D"),
        FileStatus::Unchanged => (colors::DIM, " "),
    };

    format!("{}{}{} {}", color, prefix, colors::RESET, path)
}

/// Format a change type for display
pub fn format_change_type(change_type: ChangeType, path: &str) -> String {
    let (color, prefix) = match change_type {
        ChangeType::Added => (colors::GREEN, "A"),
        ChangeType::Modified => (colors::YELLOW, "M"),
        ChangeType::Deleted => (colors::RED, "D"),
        ChangeType::Renamed => (colors::CYAN, "R"),
    };

    format!("{}{}{} {}", color, prefix, colors::RESET, path)
}

/// Canonical lowercase change name for JSON output.
pub fn change_type_name(change_type: ChangeType) -> &'static str {
    match change_type {
        ChangeType::Added => "added",
        ChangeType::Modified => "modified",
        ChangeType::Deleted => "deleted",
        ChangeType::Renamed => "renamed",
    }
}

/// Format a rename change for display (includes old_path -> new_path)
pub fn format_rename(old_path: &str, new_path: &str) -> String {
    format!(
        "{}R{} {} -> {}",
        colors::CYAN,
        colors::RESET,
        old_path,
        new_path
    )
}

/// Compact count summary for non-TTY status/commit output.
pub fn compact_change_summary(changes: &[FileChange]) -> String {
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    for change in changes {
        match change.change_type {
            ChangeType::Added => added += 1,
            ChangeType::Modified => modified += 1,
            ChangeType::Deleted => deleted += 1,
            ChangeType::Renamed => renamed += 1,
        }
    }

    let mut parts = Vec::new();
    for (n, label) in [
        (added, "added"),
        (modified, "modified"),
        (deleted, "deleted"),
        (renamed, "renamed"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }

    format!(
        "{} change{}{}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
        if parts.is_empty() {
            String::new()
        } else {
            format!(": {}", parts.join(", "))
        }
    )
}

/// One compact changed-path row, without prose.
pub fn compact_change_line(change: &FileChange) -> String {
    if change.change_type == ChangeType::Renamed {
        if let Some(old_path) = change.old_path.as_deref() {
            return format_rename(old_path, &change.path);
        }
    }
    format_change_type(change.change_type, &change.path)
}

/// Print a bounded changed-path sample after a compact summary line.
pub fn print_compact_change_sample(changes: &[FileChange], limit: usize) {
    for change in changes.iter().take(limit) {
        print_line(&compact_change_line(change));
    }
    if changes.len() > limit {
        print_line(&format!("... {} more", changes.len() - limit));
    }
}

/// One-line commit summary for piped output:
/// `<hash12>  <date>  <branch>  <subject>`.
///
/// The subject is the first non-empty line of the commit message; messageless
/// commits (the norm in oak — descriptions live on branches) fall back to the
/// changed-file count so the line still says something useful. Used by the
/// non-TTY `oak log` and the mount-aware log so both read identically.
pub fn format_commit_compact(commit: &Commit) -> String {
    let subject = commit_description_or_subject(commit);
    format!(
        "{}  {}  {}  {}",
        commit.hash.short(),
        commit.timestamp.format("%Y-%m-%d"),
        commit.branch_name,
        subject
    )
}

/// Compact piped verbose log line: one commit row plus a bounded file sample.
pub fn format_commit_compact_verbose(commit: &Commit) -> String {
    let mut line = format_commit_compact(commit);
    if !commit.files.is_empty() {
        let sample: Vec<String> = commit
            .files
            .iter()
            .take(5)
            .map(compact_change_line)
            .collect();
        line.push_str("  [");
        line.push_str(&sample.join(", "));
        if commit.files.len() > sample.len() {
            line.push_str(&format!(", ... {} more", commit.files.len() - sample.len()));
        }
        line.push(']');
    }
    line
}

/// First meaningful message line, or a file-count fallback for messageless commits.
pub fn commit_description_or_subject(commit: &Commit) -> String {
    match commit
        .message
        .as_deref()
        .and_then(|m| m.lines().find(|l| !l.trim().is_empty()))
    {
        Some(line) => line.trim().to_string(),
        None => {
            let n = commit.files.len();
            format!("{n} file{} changed", if n == 1 { "" } else { "s" })
        }
    }
}

/// The "there's more history" trailer printed after a truncated piped log.
/// Always tells the reader how to widen the window, so truncation never costs
/// an exploratory extra command.
pub fn format_log_more_hint(shown: usize) -> String {
    format!(
        "... more commits (oak log -n {} to see more)",
        shown.saturating_mul(2).max(1)
    )
}

/// Format a commit for the log display
pub fn format_commit(commit: &Commit, verbose: bool) -> String {
    let mut output = String::new();

    // Commit header
    output.push_str(&format!(
        "{}{}commit {}{}{}\n",
        colors::YELLOW,
        colors::BOLD,
        colors::RESET,
        colors::CYAN,
        commit.hash.short(),
    ));
    output.push_str(&colors::RESET.to_string());
    output.push('\n');

    // Branch
    output.push_str(&format!(
        "{}branch:{} {}{}{}\n",
        colors::DIM,
        colors::RESET,
        colors::BOLD,
        commit.branch_name,
        colors::RESET,
    ));

    // Author and date
    output.push_str(&format!(
        "{}author:{} {}\n",
        colors::DIM,
        colors::RESET,
        commit.author,
    ));
    output.push_str(&format!(
        "{}date:{} {}\n",
        colors::DIM,
        colors::RESET,
        commit.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
    ));

    // Message — only main-branch squash-merge commits have one; for every
    // other commit, the branch (and its description) carry the meaning.
    if let Some(msg) = commit.message.as_deref() {
        output.push_str(&format!("\n    {msg}\n"));
    }

    // File changes (if verbose)
    if verbose && !commit.files.is_empty() {
        output.push_str("\n    files:\n");
        for file in &commit.files {
            if file.change_type == ChangeType::Renamed {
                if let Some(ref old_path) = file.old_path {
                    output.push_str(&format!("      R {} -> {}\n", old_path, file.path));
                } else {
                    output.push_str(&format!("      R {}\n", file.path));
                }
            } else {
                output.push_str(&format!("      {} {}\n", file.change_type, file.path));
            }
        }
    }

    output
}

/// Format a branch for display
pub fn format_branch(br: &Branch, is_current: bool) -> String {
    let mut output = String::new();

    let marker = if is_current { "* " } else { "  " };
    let status_color = match br.status {
        BranchStatus::Open => colors::GREEN,
        BranchStatus::Closed => colors::DIM,
    };

    output.push_str(&format!(
        "{}{}{}{}{}{}\n",
        marker,
        colors::BOLD,
        br.name,
        colors::RESET,
        status_color,
        colors::RESET,
    ));

    if let Some(ref desc) = br.description {
        output.push_str(&format!("    {desc}\n"));
    }

    if let Some(ref parent) = br.parent_branch {
        output.push_str(&format!(
            "    {}parent: {}{}\n",
            colors::DIM,
            parent,
            colors::RESET,
        ));
    }

    output.push_str(&format!(
        "    {}[{}]{}\n",
        status_color,
        br.status,
        colors::RESET,
    ));

    output
}

/// Format diff output with colors.
///
/// Returns a borrow of `line` whenever no color wrapping happens — both in
/// no-color mode and for context lines — so large piped diffs don't pay a
/// per-line allocation just to add nothing.
pub fn format_diff_line(line: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;

    if !stdout_colors_enabled() {
        return Cow::Borrowed(line);
    }
    if line.starts_with('+') && !line.starts_with("+++") {
        Cow::Owned(format!("{}{}{}", colors::GREEN, line, colors::RESET))
    } else if line.starts_with('-') && !line.starts_with("---") {
        Cow::Owned(format!("{}{}{}", colors::RED, line, colors::RESET))
    } else if line.starts_with("@@") {
        Cow::Owned(format!("{}{}{}", colors::CYAN, line, colors::RESET))
    } else if line.starts_with("diff") || line.starts_with("---") || line.starts_with("+++") {
        Cow::Owned(format!("{}{}{}", colors::BOLD, line, colors::RESET))
    } else {
        Cow::Borrowed(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn should_color_follows_tty_by_default() {
        assert!(should_color(None, None, true));
        assert!(!should_color(None, None, false));
    }

    #[test]
    fn no_color_disables_even_on_a_tty() {
        assert!(!should_color(Some(&os("1")), None, true));
        // ...and beats CLICOLOR_FORCE.
        assert!(!should_color(Some(&os("1")), Some(&os("1")), true));
        // An empty NO_COLOR is treated as unset, per the spec.
        assert!(should_color(Some(&os("")), None, true));
    }

    #[test]
    fn clicolor_force_enables_when_piped() {
        assert!(should_color(None, Some(&os("1")), false));
        // "0" and empty mean "not forced".
        assert!(!should_color(None, Some(&os("0")), false));
        assert!(!should_color(None, Some(&os("")), false));
    }

    #[test]
    fn color_renders_its_code_only_when_enabled() {
        assert_eq!(colors::GREEN.when(true), "\x1b[32m");
        assert_eq!(colors::GREEN.when(false), "");
        assert_eq!(colors::RESET.when(false), "");
    }

    /// Under the test harness stdout is captured (not a TTY), so the
    /// process-wide decision is "no color" and every `Color` must render as
    /// nothing. Skipped if the environment forces color on.
    #[test]
    fn disabled_colors_contribute_zero_bytes_to_formatted_output() {
        if stdout_colors_enabled() {
            return; // CLICOLOR_FORCE set in the environment; nothing to assert
        }
        assert_eq!(
            format_status(FileStatus::Modified, "src/lib.rs"),
            "M src/lib.rs"
        );
        assert_eq!(format_change_type(ChangeType::Added, "a.txt"), "A a.txt");
        assert_eq!(format_rename("old.rs", "new.rs"), "R old.rs -> new.rs");
        assert_eq!(format_diff_line("+added line"), "+added line");
    }

    /// Regression test: color gating must never alter user-controlled
    /// content. A diff line whose *file content* contains literal ANSI
    /// escape bytes has to round-trip byte-for-byte — the old post-hoc
    /// stripping approach corrupted it.
    #[test]
    fn user_content_with_literal_escape_bytes_round_trips_exactly() {
        if stdout_colors_enabled() {
            return;
        }
        let user_line = "+\x1b[31mred\x1b[0m";
        // With colors disabled the wrapper adds nothing — and, crucially,
        // removes nothing.
        assert_eq!(format_diff_line(user_line), user_line);

        begin_capture();
        print_line(user_line);
        assert_eq!(end_capture(), user_line);
    }

    fn test_commit(message: Option<&str>, n_files: usize) -> Commit {
        Commit {
            hash: oak_core::Hash("abcdef123456abcdef123456".to_string()),
            branch_name: "main".to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: oak_core::Hash("0".repeat(24)),
            author: "tester".to_string(),
            message: message.map(str::to_string),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-06-09T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            files: (0..n_files)
                .map(|i| oak_core::FileChange {
                    path: format!("f{i}.txt"),
                    change_type: ChangeType::Modified,
                    old_path: None,
                    old_blob_hash: None,
                    new_blob_hash: None,
                    old_mode: None,
                    new_mode: None,
                })
                .collect(),
        }
    }

    #[test]
    fn compact_commit_line_uses_message_subject() {
        let c = test_commit(Some("subject line\nbody detail"), 3);
        assert_eq!(
            format_commit_compact(&c),
            format!("{}  2026-06-09  main  subject line", c.hash.short())
        );
    }

    #[test]
    fn compact_commit_line_falls_back_to_file_count_when_messageless() {
        let c = test_commit(None, 2);
        assert!(format_commit_compact(&c).ends_with("main  2 files changed"));
        let c = test_commit(None, 1);
        assert!(format_commit_compact(&c).ends_with("main  1 file changed"));
        // A whitespace-only message is treated as messageless too.
        let c = test_commit(Some("  \n\n"), 1);
        assert!(format_commit_compact(&c).ends_with("main  1 file changed"));
    }

    #[test]
    fn log_more_hint_doubles_the_window() {
        assert_eq!(
            format_log_more_hint(20),
            "... more commits (oak log -n 40 to see more)"
        );
        // Degenerate window still suggests something actionable.
        assert_eq!(
            format_log_more_hint(0),
            "... more commits (oak log -n 1 to see more)"
        );
    }

    #[test]
    fn captured_output_is_plain_when_colors_are_off() {
        if stdout_colors_enabled() {
            return;
        }
        begin_capture();
        success("done");
        header("Section");
        assert_eq!(end_capture(), "✓ done\nSection");
    }
}
