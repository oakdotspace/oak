//! Output-contract tests for piped (non-TTY) command output.
//!
//! Piped output is what agents and scripts read, so its shape is a contract:
//! compact, ANSI-free, bounded, and with diagnostics on stderr rather than
//! interleaved with data on stdout. These tests run the real `oak` binary
//! with piped stdio — exactly what a non-TTY consumer sees — and pin both
//! the format (golden text, normalized for hashes/dates) and the size
//! (line budgets), so a future change that quietly re-inflates the output
//! fails CI.

use std::path::Path;
use std::process::{Command, Output};

/// Run the freshly-built `oak` binary in `dir` with piped stdio and a
/// deterministic environment (no update check, fixed author, no color
/// overrides leaking in from the host).
fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Replace run-dependent values (commit hashes, dates) with placeholders so
/// outputs can be compared against golden text.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let mut norm = String::new();
        for (i, token) in line.split(' ').enumerate() {
            if i > 0 {
                norm.push(' ');
            }
            if token.len() == 12 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
                norm.push_str("HASH");
            } else if token.len() == 10
                && token.as_bytes().get(4) == Some(&b'-')
                && token.as_bytes().get(7) == Some(&b'-')
            {
                norm.push_str("DATE");
            } else {
                norm.push_str(token);
            }
        }
        out.push_str(&norm);
        out.push('\n');
    }
    out
}

/// Create a repo with two committed files and one dirty tree:
/// `src/a.txt` modified, `docs/x.md` modified.
fn fixture_repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("src/a.txt"), "a\nb\n").unwrap();
    std::fs::write(dir.join("docs/x.md"), "x\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("src/a.txt"), "a\nB\nc\n").unwrap();
    std::fs::write(dir.join("docs/x.md"), "x\ny\n").unwrap();
    temp
}

/// Every piped output must be free of ANSI escapes — that contract comes from
/// the render-time color gating and this is its end-to-end check.
fn assert_no_ansi(label: &str, s: &str) {
    assert!(
        !s.contains('\u{1b}'),
        "{label} must not contain ANSI escapes when piped, got: {s:?}"
    );
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn piped_status_is_compact_and_ansi_free() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["status"]);
    let text = stdout(&out);
    assert_no_ansi("status", &text);

    assert!(text.contains("2 changes: 2 modified"), "got: {text}");
    assert!(text.contains("M docs/x.md"), "got: {text}");
    assert!(text.contains("M src/a.txt"), "got: {text}");

    // Size budget: a two-file dirty status fits in a small, fixed window
    // (summary + bounded path sample).
    let lines = text.lines().count();
    assert!(lines <= 3, "status grew to {lines} lines:\n{text}");
}

#[test]
fn piped_status_omits_branch_description_noise() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["desc", "Subject line\nbody one\nbody two"]);
    assert!(out.status.success(), "desc failed: {}", stderr(&out));

    let text = stdout(&oak(temp.path(), &["status"]));
    assert!(!text.contains("Description:"), "got: {text}");
    assert!(
        !text.contains("Subject line") && !text.contains("body one") && !text.contains("body two"),
        "description leaked into status: {text}"
    );
    assert!(
        text.contains("2 changes: 2 modified"),
        "status summary missing: {text}"
    );
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

#[test]
fn piped_log_is_one_line_per_commit() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["log"]);
    let text = stdout(&out);
    assert_no_ansi("log", &text);

    // One commit, one line: `<hash12>  <date>  <branch>  <subject>`. The
    // branch is the auto-generated personal branch, so assert the line's
    // structure rather than a golden branch name.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "one commit must be one line:\n{text}");
    let fields: Vec<&str> = lines[0].split("  ").collect();
    assert_eq!(fields.len(), 4, "4 double-space fields, got: {}", lines[0]);
    assert_eq!(normalize(fields[0]).trim(), "HASH", "got: {}", fields[0]);
    assert_eq!(normalize(fields[1]).trim(), "DATE", "got: {}", fields[1]);
    assert!(!fields[2].is_empty(), "branch field empty: {}", lines[0]);
    assert_eq!(fields[3], "2 files changed");
}

#[test]
fn piped_log_defaults_to_a_bounded_window_with_more_hint() {
    let temp = fixture_repo();
    let dir = temp.path();
    // 22 commits total (1 from the fixture + 21 here) exceeds the default
    // window of 20.
    for i in 0..21 {
        std::fs::write(dir.join("churn.txt"), format!("{i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
    }

    let text = stdout(&oak(dir, &["log"]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        21,
        "expected 20 commits + hint, got {} lines:\n{text}",
        lines.len()
    );
    assert_eq!(
        lines.last().copied().unwrap(),
        "... more commits (oak log -n 40 to see more)"
    );

    // Explicit -n always wins over the default window.
    let text = stdout(&oak(dir, &["log", "-n", "2"]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "2 commits + hint:\n{text}");
    assert_eq!(
        lines.last().copied().unwrap(),
        "... more commits (oak log -n 4 to see more)"
    );
}

#[test]
fn piped_log_verbose_is_compact_with_files() {
    let temp = fixture_repo();
    let text = stdout(&oak(temp.path(), &["log", "-v"]));
    assert_no_ansi("log -v", &text);
    assert!(!text.contains("author: tester"), "got: {text}");
    assert!(text.contains("files changed"), "got: {text}");
    assert!(text.contains("[A docs/x.md"), "got: {text}");
    assert!(text.lines().all(|line| !line.is_empty()), "got: {text}");
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

#[test]
fn piped_diff_print_matches_golden() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["diff", "--print", "src"]);
    let text = stdout(&out);
    assert_no_ansi("diff --print", &text);

    // Scoped to src/, so docs/x.md must not appear.
    assert_eq!(
        text,
        "diff --oak a/src/a.txt b/src/a.txt\n\
         --- a/src/a.txt\n\
         +++ b/src/a.txt\n\
         @@ -1,2 +1,3 @@\n \
         a\n\
         -b\n\
         +B\n\
         +c\n\n"
    );
}

#[test]
fn diff_path_scoping_filters_files_and_reports_empty_scopes() {
    let temp = fixture_repo();

    let text = stdout(&oak(temp.path(), &["diff", "--print", "docs/x.md"]));
    assert!(
        text.contains("diff --oak a/docs/x.md b/docs/x.md"),
        "got: {text}"
    );
    assert!(!text.contains("src/a.txt"), "scope leaked: {text}");

    let text = stdout(&oak(temp.path(), &["diff", "--print", "no-such-dir"]));
    assert_eq!(text, "No differences in the given paths\n");
}

#[test]
fn diff_stat_summarizes_per_file_counts() {
    let temp = fixture_repo();
    let text = stdout(&oak(temp.path(), &["diff", "--stat"]));
    assert_no_ansi("diff --stat", &text);
    assert_eq!(
        text,
        "M docs/x.md  +1 -0\n\
         M src/a.txt  +2 -1\n\
         2 files changed, +3 -1\n"
    );
}

#[test]
fn diff_stat_counts_content_lines_that_look_like_file_headers() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("file.txt"), "-- removed sql comment\nkeep\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("file.txt"), "++i;\nkeep\n").unwrap();

    let text = stdout(&oak(dir, &["diff", "--stat"]));
    assert_no_ansi("diff --stat", &text);
    assert_eq!(text, "M file.txt  +1 -1\n1 file changed, +1 -1\n");
}

// ---------------------------------------------------------------------------
// diagnostics channel
// ---------------------------------------------------------------------------

#[test]
fn errors_go_to_stderr_not_stdout() {
    let temp = tempfile::TempDir::new().unwrap();
    // No repo here: `oak log` must fail with a clean stdout and the error on
    // stderr, so piped consumers never parse diagnostics as data.
    let out = oak(temp.path(), &["log"]);
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "", "stdout must stay clean");
    assert!(
        stderr(&out).contains("error:"),
        "expected error on stderr, got: {}",
        stderr(&out)
    );
}
