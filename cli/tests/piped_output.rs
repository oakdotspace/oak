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

fn oak_with_stdin(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oak binary should spawn");

    {
        let mut child_stdin = child.stdin.take().expect("stdin should be piped");
        std::io::Write::write_all(&mut child_stdin, stdin.as_bytes())
            .expect("stdin should be writable");
    }

    child.wait_with_output().expect("oak binary should run")
}

fn oak_with_forced_color(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
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

#[test]
fn top_level_help_prefers_file_based_description_flow() {
    let temp = tempfile::TempDir::new().unwrap();

    let out = oak(temp.path(), &["--help"]);

    assert!(
        out.status.success(),
        "help should exit successfully\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let help = stdout(&out);
    assert!(help.contains("desc"));
    assert!(help.contains("--file FILE"));
    assert!(help.contains("finish"));
    assert!(help.contains("--desc-file FILE [--json]"));
    assert!(
        !help.contains("--desc TEXT [--json]"),
        "top-level help should steer agents away from shell-quoted descriptions:\n{help}"
    );
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
            if (token.len() == 8 || token.len() == 12)
                && token.bytes().all(|b| b.is_ascii_hexdigit())
            {
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

fn many_dirty_files_repo(count: usize) -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::create_dir_all(dir.join("bulk")).unwrap();
    for i in 0..count {
        std::fs::write(dir.join(format!("bulk/file-{i:02}.txt")), "before\n").unwrap();
    }
    assert!(oak(dir, &["commit"]).status.success());
    for i in 0..count {
        std::fs::write(dir.join(format!("bulk/file-{i:02}.txt")), "before\nafter\n").unwrap();
    }
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

fn current_description(dir: &Path) -> String {
    let out = oak(dir, &["branch", "list", "--json"]);
    assert!(out.status.success(), "branch list failed: {}", stderr(&out));
    let rows: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("branch list should emit JSON");
    rows.as_array()
        .and_then(|branches| {
            branches
                .iter()
                .find(|branch| branch["current"].as_bool() == Some(true))
        })
        .and_then(|branch| branch["description"].as_str())
        .expect("current branch should have a description")
        .to_string()
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

    // No summary header — the per-file rows are the whole output.
    assert_eq!(text, "M docs/x.md\nM src/a.txt\n", "got: {text}");
}

#[test]
fn status_porcelain_is_compact_stable_and_ansi_free() {
    let temp = fixture_repo();
    let out = oak_with_forced_color(temp.path(), &["status", "--porcelain"]);
    assert!(
        out.status.success(),
        "status --porcelain failed: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert_no_ansi("status --porcelain", &text);
    assert_eq!(text, "M docs/x.md\nM src/a.txt\n", "got: {text}");
}

#[test]
fn status_short_aliases_porcelain_rows() {
    let temp = fixture_repo();
    let out = oak_with_forced_color(temp.path(), &["status", "--short"]);
    assert!(
        out.status.success(),
        "status --short failed: {}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert_no_ansi("status --short", &text);
    assert_eq!(text, "M docs/x.md\nM src/a.txt\n", "got: {text}");
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
        text.contains("M docs/x.md") && text.contains("M src/a.txt"),
        "changed-path rows missing: {text}"
    );
}

#[test]
fn piped_status_lists_every_changed_path() {
    let temp = many_dirty_files_repo(8);
    let text = stdout(&oak(temp.path(), &["status"]));

    assert_no_ansi("status", &text);
    assert!(
        !text.contains("..."),
        "piped status must not replace path evidence with a summary: {text}"
    );
    for i in 0..8 {
        let path = format!("M bulk/file-{i:02}.txt");
        assert!(text.contains(&path), "missing {path}: {text}");
    }
}

#[test]
fn info_without_json_is_concise_orientation_output() {
    let temp = fixture_repo();
    let desc =
        "Short useful summary\n\nLonger branch narrative that belongs in JSON or desc output.";
    assert!(oak(temp.path(), &["desc", desc]).status.success());

    let out = oak(temp.path(), &["info"]);
    assert!(out.status.success(), "info failed: {}", stderr(&out));
    let text = stdout(&out);
    assert_no_ansi("info", &text);

    assert!(text.contains("Repository: (unlinked)"), "info: {text}");
    assert!(text.contains("Remote: (none)"), "info: {text}");
    assert!(text.contains("Branch: tester-"), "info: {text}");
    assert!(text.contains("Parent: main"), "info: {text}");
    assert!(text.contains("Head: "), "info: {text}");
    assert!(text.contains("Status: open"), "info: {text}");
    assert!(
        text.contains("Description: Short useful summary"),
        "info: {text}"
    );
    assert!(
        !text.contains("Longer branch narrative"),
        "plain info should not dump multiline branch descriptions: {text}"
    );
    assert!(text.contains("Progress: none"), "info: {text}");
}

#[test]
fn git_head_aliases_are_low_token_and_stable() {
    let temp = fixture_repo();
    let hash = stdout(&oak(temp.path(), &["hash"]));
    let hash = hash.trim_end();

    let rev_parse = stdout(&oak(temp.path(), &["rev-parse", "HEAD"]));
    assert_eq!(rev_parse.trim_end(), hash);

    let short = stdout(&oak(temp.path(), &["rev-parse", "--short", "HEAD"]));
    let short = short.trim_end();
    assert_eq!(short.len(), 12);
    assert!(hash.starts_with(short));

    let branch = stdout(&oak(temp.path(), &["branch", "--show-current"]));
    assert!(branch.trim_end().starts_with("tester-"), "branch: {branch}");
}

#[test]
fn rev_parse_refuses_unsupported_revs_without_guessing() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["rev-parse", "main"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty(), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("supports only HEAD"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn desc_file_reads_multiline_markdown_exactly() {
    let temp = fixture_repo();
    let description = "# Summary\n\n- preserves `inline code`\n\n```rust\nfn main() {}\n```\n";
    std::fs::write(temp.path().join("description.md"), description).unwrap();

    let out = oak(temp.path(), &["desc", "--file", "description.md"]);
    assert!(out.status.success(), "desc failed: {}", stderr(&out));

    assert_eq!(current_description(temp.path()), description);
}

#[test]
fn desc_file_dash_reads_stdin_multiline_markdown_exactly() {
    let temp = fixture_repo();
    let description = "Subject line\n\nBody with `backticks`.\n\n```md\n# fenced markdown\n```\nno trailing newline";

    let out = oak_with_stdin(temp.path(), &["desc", "--file", "-"], description);
    assert!(out.status.success(), "desc failed: {}", stderr(&out));

    assert_eq!(current_description(temp.path()), description);
}

#[test]
fn desc_file_rejects_positional_description_too() {
    let temp = fixture_repo();
    std::fs::write(temp.path().join("description.md"), "from file").unwrap();

    let out = oak(temp.path(), &["desc", "inline", "--file", "description.md"]);
    assert!(
        !out.status.success(),
        "ambiguous desc unexpectedly succeeded"
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}

// ---------------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------------

#[test]
fn piped_commit_is_hash_only() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["commit"]);
    assert!(out.status.success(), "commit failed: {}", stderr(&out));
    let text = stdout(&out);
    assert_no_ansi("commit", &text);

    // One line, hash only: the committer just chose what to commit, and the
    // exit code carries success/failure, so the command prefix and change
    // counts are redundant for a piped reader.
    let line = text.trim_end();
    assert!(
        line.len() == 12 && line.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected '<hash12>', got: {text:?}"
    );
}

#[test]
fn commit_message_flag_explains_branch_descriptions() {
    let temp = fixture_repo();

    for args in [
        ["commit", "-m", "message"].as_slice(),
        ["commit", "--message", "message"].as_slice(),
    ] {
        let out = oak(temp.path(), args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} should be a usage error, stderr: {}",
            stderr(&out)
        );
        assert_eq!(stdout(&out), "", "usage errors should not print stdout");
        let err = stderr(&out);
        assert!(
            err.contains("oak commit does not take a message"),
            "stderr: {err}"
        );
        assert!(
            err.contains("oak desc --file <file>"),
            "stderr should name the branch-description path: {err}"
        );
    }

    let log = oak(temp.path(), &["log", "--json"]);
    assert!(log.status.success(), "log failed: {}", stderr(&log));
    let commits: serde_json::Value =
        serde_json::from_slice(&log.stdout).expect("log should emit JSON");
    assert_eq!(
        commits.as_array().unwrap().len(),
        1,
        "refused commit -m must not create a commit"
    );
}

// ---------------------------------------------------------------------------
// switch -c (branch create)
// ---------------------------------------------------------------------------

#[test]
fn piped_branch_create_is_one_short_line() {
    let temp = fixture_repo();
    let out = oak(temp.path(), &["switch", "-c", "exp1"]);
    assert!(out.status.success(), "switch -c failed: {}", stderr(&out));
    let text = stdout(&out);
    assert_no_ansi("switch -c", &text);

    // No "from 'main'": every oak branch parents onto main, so the suffix is
    // constant information a piped reader never needs.
    assert_eq!(text, "Created branch 'exp1'\n", "got: {text}");
    // Byte budget: stay under git's `Switched to a new branch 'exp1'` (38).
    assert!(text.len() <= 28, "branch create output grew: {text:?}");
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

    // One commit, one line: `<hash8> <date> <subject>`. The commit sits on
    // the branch being logged, so no branch column — repeating the current
    // branch on every row is pure context cost for piped readers.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "one commit must be one line:\n{text}");
    assert_eq!(normalize(lines[0]).trim_end(), "HASH DATE 2 files");
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
        "... more commits: oak log -n 40"
    );

    // Explicit -n always wins over the default window, and gets no hint:
    // the reader chose the window, so echoing one back is pure context cost.
    let text = stdout(&oak(dir, &["log", "-n", "2"]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "exactly 2 commits, no hint:\n{text}");
    assert!(
        !text.contains("more commits"),
        "explicit -n must not hint:\n{text}"
    );
}

#[test]
fn log_oneline_is_compact_and_accepts_explicit_limit() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("churn.txt"), "one\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("churn.txt"), "two\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let text = stdout(&oak(dir, &["log", "--oneline", "-n", "2"]));
    assert_no_ansi("log --oneline", &text);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "exactly 2 compact commits:\n{text}");
    assert_eq!(normalize(lines[0]).trim_end(), "HASH DATE 1 file");
    assert_eq!(normalize(lines[1]).trim_end(), "HASH DATE 3 files");
    assert!(
        !text.contains("more commits"),
        "explicit -n must not hint:\n{text}"
    );
}

#[test]
fn log_oneline_rejects_ambiguous_modes() {
    let temp = fixture_repo();
    let json = oak(temp.path(), &["log", "--oneline", "--json"]);
    assert_eq!(json.status.code(), Some(2), "stderr: {}", stderr(&json));

    let verbose = oak(temp.path(), &["log", "--oneline", "-v"]);
    assert_eq!(
        verbose.status.code(),
        Some(2),
        "stderr: {}",
        stderr(&verbose)
    );
}

#[test]
fn piped_log_verbose_is_compact_with_files() {
    let temp = fixture_repo();
    let text = stdout(&oak(temp.path(), &["log", "-v"]));
    assert_no_ansi("log -v", &text);
    assert!(!text.contains("author: tester"), "got: {text}");
    // Commit row, then one unindented `<letter> <path>` row per file. The
    // messageless file-count fallback is omitted: the rows below carry it.
    assert_eq!(normalize(&text), "HASH DATE\nA docs/x.md\nA src/a.txt\n");
}

#[test]
fn log_filters_history_by_path() {
    let temp = fixture_repo();
    let dir = temp.path();
    // Commit the fixture's dirty edits (touches both files), then one
    // src-only and one docs-only commit.
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("src/a.txt"), "a\nB\nc\nd\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("docs/x.md"), "x\ny\nz\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    // File path: every commit that touched src/a.txt, not the docs-only one.
    let text = stdout(&oak(dir, &["log", "src/a.txt"]));
    let commit_rows = text
        .lines()
        .filter(|l| normalize(l).starts_with("HASH"))
        .count();
    assert_eq!(commit_rows, 3, "got:\n{text}");
    let all = stdout(&oak(dir, &["log"]));
    assert_eq!(all.lines().count(), 4, "got:\n{all}");

    // Directory prefix works the same way.
    let by_dir = stdout(&oak(dir, &["log", "src"]));
    assert_eq!(by_dir, text, "dir prefix and file filter should agree here");

    let from_subdir = stdout(&oak(&dir.join("src"), &["log", "a.txt"]));
    assert_eq!(
        from_subdir, text,
        "cwd-relative file filters should resolve to repo-relative paths"
    );

    // No matches is said in one short line.
    let text = stdout(&oak(dir, &["log", "no/such/path"]));
    assert_eq!(text, "No matching commits\n");
}

#[test]
fn path_filtered_log_limit_returns_latest_matches_without_full_history_hint() {
    let temp = fixture_repo();
    let dir = temp.path();

    for i in 0..5 {
        std::fs::write(dir.join("src/a.txt"), format!("src {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
        std::fs::write(dir.join("docs/x.md"), format!("docs {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
    }

    let text = stdout(&oak(dir, &["log", "src/a.txt", "-n", "2"]));
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "explicit path-filtered limit:\n{text}");
    assert!(
        !text.contains("more commits"),
        "explicit filtered limit must not emit implicit-window hint:\n{text}"
    );
}

#[test]
fn path_filtered_piped_log_keeps_more_hint_when_matches_exceed_default_window() {
    let temp = fixture_repo();
    let dir = temp.path();

    for i in 0..21 {
        std::fs::write(dir.join("src/a.txt"), format!("src {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
        std::fs::write(dir.join("docs/x.md"), format!("docs {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
    }

    let text = stdout(&oak(dir, &["log", "src/a.txt"]));
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(
        rows.len(),
        21,
        "expected 20 matching commits plus hint, got {} rows:\n{text}",
        rows.len()
    );
    assert_eq!(
        rows.last().copied().unwrap(),
        "... more commits: oak log -n 40"
    );
}

#[test]
fn log_pickaxe_finds_commits_changing_term_count() {
    let temp = fixture_repo();
    let dir = temp.path();
    // Commit the dirty fixture edits (introduces "B"), then an unrelated one.
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("docs/x.md"), "x\ny\nz\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    // "B" appears only in the second commit's change to src/a.txt.
    let text = stdout(&oak(dir, &["log", "-v", "-S", "B"]));
    let commit_rows = text
        .lines()
        .filter(|l| normalize(l).starts_with("HASH"))
        .count();
    assert_eq!(commit_rows, 1, "got:\n{text}");
    assert!(text.contains("M src/a.txt"), "got:\n{text}");

    // A term whose count never changes after its introduction matches only
    // the commit that introduced it — count deltas, not mere mentions.
    let text = stdout(&oak(dir, &["log", "-S", "x"]));
    let commit_rows = text
        .lines()
        .filter(|l| normalize(l).starts_with("HASH"))
        .count();
    assert_eq!(commit_rows, 1, "only the introducing commit:\n{text}");

    let text = stdout(&oak(dir, &["log", "-S", "no-such-term"]));
    assert_eq!(text, "No matching commits\n");
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
fn diff_name_only_lists_filtered_paths_without_counts() {
    let temp = fixture_repo();
    let text = stdout(&oak(temp.path(), &["diff", "--name-only", "src"]));
    assert_no_ansi("diff --name-only", &text);
    assert_eq!(text, "src/a.txt\n", "got: {text}");
    assert!(!text.contains("docs/x.md"), "scope leaked: {text}");
    assert!(
        !text.contains('+') && !text.contains('-'),
        "counts leaked: {text}"
    );
}

#[test]
fn diff_name_only_rejects_ambiguous_modes() {
    let temp = fixture_repo();
    for args in [
        ["diff", "--name-only", "--json"].as_slice(),
        ["diff", "--name-only", "--print"].as_slice(),
        ["diff", "--name-only", "--stat"].as_slice(),
    ] {
        let out = oak(temp.path(), args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} should be a usage error, stderr: {}",
            stderr(&out)
        );
    }
}

#[test]
fn piped_diff_lists_every_changed_path() {
    let temp = many_dirty_files_repo(8);
    let text = stdout(&oak(temp.path(), &["diff"]));

    assert_no_ansi("diff", &text);
    assert!(
        !text.contains("..."),
        "piped diff must not replace path evidence with a summary: {text}"
    );
    for i in 0..8 {
        let path = format!("M bulk/file-{i:02}.txt  +1 -0");
        assert!(text.contains(&path), "missing {path}: {text}");
    }
    assert!(
        text.contains("8 files changed, +8 -0"),
        "missing totals line: {text}"
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

#[test]
fn diff_stat_keeps_binary_rows_without_text_counts() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("asset.bin"), [0x00, 0x01, 0x02]).unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("asset.bin"), [0x00, 0x03, 0x04]).unwrap();

    let text = stdout(&oak(dir, &["diff", "--stat"]));
    assert_no_ansi("diff --stat", &text);
    assert_eq!(text, "M asset.bin  bin\n1 file changed, +0 -0\n");
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
