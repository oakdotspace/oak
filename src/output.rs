#![allow(dead_code)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use oak_core::{Branch, BranchStatus, ChangeType, Commit, FileStatus};

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
        colors::DIM,
        elapsed.as_secs_f64(),
        colors::RESET,
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

/// Print a plain line (replaces direct `println!` calls in command modules).
pub fn print_line(msg: &str) {
    emit_line(msg);
}

/// ANSI color codes
pub mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
    pub const WHITE: &str = "\x1b[37m";
    pub const BRIGHT_GREEN: &str = "\x1b[92m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const UNDERLINE: &str = "\x1b[4m";
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

/// Print a warning message
pub fn warning(msg: &str) {
    emit_line(&format!(
        "{}{}warning:{} {}",
        colors::YELLOW,
        colors::BOLD,
        colors::RESET,
        msg
    ));
}

/// Print an error message
pub fn error(msg: &str) {
    emit_line(&format!(
        "{}{}error:{} {}",
        colors::RED,
        colors::BOLD,
        colors::RESET,
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
    output.push_str(colors::RESET);
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

/// Format diff output with colors
pub fn format_diff_line(line: &str) -> String {
    if line.starts_with('+') && !line.starts_with("+++") {
        format!("{}{}{}", colors::GREEN, line, colors::RESET)
    } else if line.starts_with('-') && !line.starts_with("---") {
        format!("{}{}{}", colors::RED, line, colors::RESET)
    } else if line.starts_with("@@") {
        format!("{}{}{}", colors::CYAN, line, colors::RESET)
    } else if line.starts_with("diff") || line.starts_with("---") || line.starts_with("+++") {
        format!("{}{}{}", colors::BOLD, line, colors::RESET)
    } else {
        line.to_string()
    }
}
