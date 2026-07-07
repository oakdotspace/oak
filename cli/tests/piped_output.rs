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
    // The hint carries the path filter: following it must reproduce the
    // same filtered query, not the unfiltered history.
    assert_eq!(
        rows.last().copied().unwrap(),
        "... more commits: oak log -n 40 src/a.txt"
    );
}

#[test]
fn pickaxe_filtered_piped_log_hint_reproduces_the_query() {
    let temp = fixture_repo();
    let dir = temp.path();

    // 21 commits whose diffs each add a line matching the pattern — more
    // matches than the default piped window of 20.
    for i in 0..21 {
        std::fs::write(dir.join("src/a.txt"), format!("src {i}\n")).unwrap();
        assert!(oak(dir, &["commit"]).status.success());
    }

    let text = stdout(&oak(dir, &["log", "-G", r"src \d+"]));
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(
        rows.len(),
        21,
        "expected 20 matching commits plus hint, got {} rows:\n{text}",
        rows.len()
    );
    // The hint must reproduce the -G filter, shell-quoted, with a doubled
    // window — an agent following a filterless hint would land on
    // unrelated commits.
    assert_eq!(
        rows.last().copied().unwrap(),
        r"... more commits: oak log -G 'src \d+' -n 40"
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

/// Count the commit rows in compact piped `oak log` output.
fn commit_row_count(text: &str) -> usize {
    text.lines()
        .filter(|l| normalize(l).starts_with("HASH"))
        .count()
}

#[test]
fn log_regex_pickaxe_matches_added_lines_a_literal_search_cannot_express() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    // This commit's diff adds a line only a regex can describe generically.
    std::fs::write(dir.join("a.rs"), "fn main() {}\nfn handle_error_42() {}\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("b.txt"), "unrelated\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let pattern = r"fn handle_\w+_\d+";
    // -S treats the pattern as a literal string: no such substring exists.
    let text = stdout(&oak(dir, &["log", "-S", pattern]));
    assert_eq!(text, "No matching commits\n");

    // -G matches the regex against the diff's added lines.
    let out = oak(dir, &["log", "-v", "-G", pattern]);
    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(commit_row_count(&text), 1, "got:\n{text}");
    assert!(text.contains("M a.rs"), "got:\n{text}");
}

#[test]
fn log_regex_pickaxe_matches_moved_term_that_count_based_search_misses() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    // Move "alpha" without changing its total occurrence count: -S sees no
    // count delta by design, but the diff removes and re-adds the line.
    std::fs::write(dir.join("a.txt"), "beta\ngamma\nalpha\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let text = stdout(&oak(dir, &["log", "-S", "alpha"]));
    assert_eq!(
        commit_row_count(&text),
        1,
        "-S matches only the introducing commit:\n{text}"
    );

    let text = stdout(&oak(dir, &["log", "-G", "alpha"]));
    assert_eq!(
        commit_row_count(&text),
        2,
        "-G matches the introduction and the move:\n{text}"
    );
}

#[test]
fn log_regex_pickaxe_rejects_invalid_pattern_as_usage_error() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());

    let out = oak(dir, &["log", "-G", "unclosed["]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "invalid regex is a usage error\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("invalid regex") && err.contains("unclosed["),
        "error should name the flag's regex problem clearly:\n{err}"
    );
}

#[test]
fn log_literal_and_regex_pickaxe_flags_conflict() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());

    let out = oak(dir, &["log", "-S", "term", "-G", "term"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "-S with -G is a usage error\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("--search") && err.contains("--search-regex"),
        "clap should name the conflicting flags:\n{err}"
    );
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

    // A leading argument that is neither a revision nor an existing path is
    // treated as a typo (like git), with `--` as the documented escape
    // hatch for paths that no longer exist on disk.
    let out = oak(temp.path(), &["diff", "--print", "no-such-dir"]);
    assert_eq!(out.status.code(), Some(2), "usage error expected");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("'no-such-dir' is not a branch, commit, or existing path"),
        "got: {err}"
    );

    let text = stdout(&oak(temp.path(), &["diff", "--print", "--", "no-such-dir"]));
    assert_eq!(text, "No differences in the given paths\n");
}

/// A repo with two commits and a feature branch, for endpoint diffs.
fn endpoint_fixture_repo() -> (tempfile::TempDir, String, String) {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("base.txt"), "alpha\nbeta\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let first = stdout(&oak(dir, &["hash"])).trim().to_string();
    assert!(oak(dir, &["switch", "-c", "feature"]).status.success());
    std::fs::write(dir.join("base.txt"), "alpha\nBETA\ngamma\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let second = stdout(&oak(dir, &["hash"])).trim().to_string();
    (temp, first, second)
}

#[test]
fn diff_between_two_commits_prints_hunks_checkout_free() {
    let (temp, first, second) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", &first[..8], &second[..8], "--print"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("-beta"), "got: {text}");
    assert!(text.contains("+BETA"), "got: {text}");
    assert!(text.contains("+gamma"), "got: {text}");
}

#[test]
fn diff_single_branch_defaults_to_contribution_and_stat_fallback_is_self_describing() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", "feature"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("M base.txt"), "got: {text}");
    // Piped output degrades to stat + a footer naming the exact commands
    // that fetch more — callers never guess flags. The invocation is the
    // canonical resolved-explicit form (defaults spelled out), identical to
    // the stubs in the JSON payload.
    assert!(
        text.contains("# hunks: oak diff feature --against main --mode contribution --print"),
        "got: {text}"
    );
}

#[test]
fn diff_json_command_stubs_match_the_human_footer_spelling() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", "feature", "--json"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let recommended: Vec<&str> = json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command.as_str().unwrap())
        .collect();
    // Same canonical resolved-explicit invocation the human footer prints.
    assert!(
        recommended.contains(&"oak diff feature --against main --mode contribution --json --hunks"),
        "got: {recommended:?}"
    );
    assert!(
        recommended.contains(&"oak diff feature --against main --mode contribution --print"),
        "got: {recommended:?}"
    );
}

#[test]
fn diff_single_branch_json_reports_endpoint_diff_kind() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", "feature", "--json"]);
    assert!(out.status.success());
    let text = stdout(&out);
    let json: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(json["kind"], "endpoint_diff");
    assert_eq!(json["diff_mode"], "contribution");
    assert_eq!(json["branch"], "feature");
    assert_eq!(json["changed_file_count"], 1);
    assert_eq!(json["changed_files"][0]["path"], "base.txt");
    // Endpoint diffs do not compute lineage; the field is omitted rather
    // than emitted as an all-null stub that fakes real zeros.
    assert!(json.get("merge_lineage_evidence").is_none(), "got: {json}");
}

#[test]
fn endpoint_diff_reports_rename_with_edit_as_renamed() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(
        dir.join("old.txt"),
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    )
    .unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let first = stdout(&oak(dir, &["hash"])).trim().to_string();
    assert!(oak(dir, &["switch", "-c", "renamer"]).status.success());
    std::fs::remove_file(dir.join("old.txt")).unwrap();
    std::fs::write(
        dir.join("new.txt"),
        "line one\nline two\nline three\nline four\nline five\nline changed\n",
    )
    .unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let second = stdout(&oak(dir, &["hash"])).trim().to_string();

    // Rename-with-edit parity: the revision-endpoint forms must report the
    // same single R entry as `oak branch diff --json` — never D+A.
    let assert_renamed = |json: &serde_json::Value| {
        let changed = json["changed_files"].as_array().expect("changed_files");
        assert_eq!(
            changed.len(),
            1,
            "rename-with-edit must be one entry, not D+A: {json}"
        );
        assert_eq!(changed[0]["status"], "renamed");
        assert_eq!(changed[0]["path"], "new.txt");
        assert_eq!(changed[0]["old_path"], "old.txt");
    };

    // Single-branch endpoint (contribution mode).
    let out = oak(dir, &["diff", "renamer", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_renamed(&json);

    // Commit-pair endpoint (tree diff).
    let out = oak(dir, &["diff", &first[..8], &second[..8], "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_renamed(&json);
}

#[test]
fn diff_treats_rowless_main_as_a_branch_endpoint() {
    let (temp, _, _) = endpoint_fixture_repo();
    // A fresh init has no local branch row for `main` (Oak treats main as
    // server-owned), but `main` is still a branch endpoint — never a
    // "not a branch, commit, or existing path" usage error.
    let out = oak(temp.path(), &["diff", "main", "feature"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("M base.txt"), "got: {text}");
    // The against-side fallback caveat surfaces on stderr, like
    // `--against main` always did.
    assert!(
        stderr(&out).contains("no local head"),
        "got: {}",
        stderr(&out)
    );

    // `main` works in either endpoint position and alone.
    let out = oak(temp.path(), &["diff", "feature", "main"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = oak(temp.path(), &["diff", "main", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
}

#[test]
fn diff_json_hunks_attaches_patches_per_file() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", "feature", "--json", "--hunks"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let patch = json["changed_files"][0]["patch"]
        .as_str()
        .expect("patch attached");
    assert!(patch.contains("-beta"), "got: {patch}");
    assert!(patch.contains("+BETA"), "got: {patch}");
    assert!(json.get("hunks_truncated").is_none(), "not truncated");
}

#[test]
fn diff_json_hunks_max_bytes_truncates_with_resume_command() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(
        temp.path(),
        &["diff", "feature", "--json", "--hunks", "--max-bytes", "10"],
    );
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(json["hunks_truncated"], true);
    assert_eq!(json["changed_files"][0]["patch_omitted"], true);
    assert!(json["changed_files"][0].get("patch").is_none());
    // The payload names the exact per-file fetch command.
    let recommended = json["recommended_next_commands"][0].as_str().unwrap();
    assert!(
        recommended.contains("--json --hunks -- base.txt"),
        "got: {recommended}"
    );
}

#[test]
fn diff_json_classifies_noise_files_and_omits_source_category() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("main.rs"), "fn main() { run() }\n").unwrap();
    std::fs::write(dir.join("Cargo.lock"), "name = \"x\"\n").unwrap();
    std::fs::create_dir_all(dir.join("vendor")).unwrap();
    std::fs::write(dir.join("vendor/dep.js"), "x\n").unwrap();
    std::fs::write(dir.join("gen.rs"), "// @generated by tool\n").unwrap();

    let out = oak(dir, &["diff", "--json"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let by_path: std::collections::HashMap<&str, &serde_json::Value> = json["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| (file["path"].as_str().unwrap(), file))
        .collect();
    assert_eq!(by_path["Cargo.lock"]["category"], "lockfile");
    assert_eq!(by_path["vendor/dep.js"]["category"], "vendored");
    assert_eq!(by_path["gen.rs"]["category"], "generated");
    // Ordinary source omits the field entirely: absent means source.
    assert!(by_path["main.rs"].get("category").is_none());
}

#[test]
fn diff_exit_code_reports_differences_without_output_parsing() {
    let (temp, first, second) = endpoint_fixture_repo();
    let same = oak(
        temp.path(),
        &["diff", &first[..8], &first[..8], "--exit-code"],
    );
    assert_eq!(same.status.code(), Some(0));
    let differs = oak(
        temp.path(),
        &["diff", &first[..8], &second[..8], "--exit-code"],
    );
    assert_eq!(differs.status.code(), Some(1));
}

#[test]
fn word_diff_marks_intra_line_changes_with_plain_markers() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("f.txt"), "the quick brown fox jumps\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("f.txt"), "the quick red fox leaps\n").unwrap();

    let text = stdout(&oak(dir, &["diff", "--print", "--word-diff"]));
    assert!(text.contains("[-brown-]"), "got: {text}");
    assert!(text.contains("{+red+}"), "got: {text}");
    // Unchanged words stay unmarked.
    assert!(text.contains("the quick"), "got: {text}");
}

#[test]
fn unified_context_flag_controls_hunk_width() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("f.txt"), "one\ntwo\nTHREE\nfour\nfive\n").unwrap();

    let zero = stdout(&oak(dir, &["diff", "--print", "-U", "0"]));
    assert!(zero.contains("+THREE"), "got: {zero}");
    assert!(!zero.contains(" two"), "no context expected: {zero}");

    let one = stdout(&oak(dir, &["diff", "--print", "-U", "1"]));
    assert!(one.contains(" two"), "got: {one}");
    assert!(!one.contains(" one"), "only one context line: {one}");
}

/// Two sibling branches (`conf-a`, `conf-b`) editing the same line from a
/// shared fork point — the merge prediction reports base.txt as a conflict.
fn conflicting_branches_repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("base.txt"), "alpha\nbeta\ngamma\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", "-c", "conf-a"]).status.success());
    assert!(oak(dir, &["switch", "-c", "conf-b"]).status.success());
    std::fs::write(dir.join("base.txt"), "alpha\nBBB\ngamma\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", "conf-a"]).status.success());
    std::fs::write(dir.join("base.txt"), "alpha\nAAA\ngamma\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    temp
}

#[test]
fn net_merge_diff_names_predicted_conflict_files_instead_of_hiding_them() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();

    let out = oak(
        dir,
        &["diff", "conf-a", "conf-b", "--mode", "net-merge", "--json"],
    );
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let conflicts = json["conflict_files"].as_array().expect("conflicts named");
    assert!(
        conflicts.iter().any(|path| path == "base.txt"),
        "got: {json}"
    );
    assert_eq!(json["conflict_file_count"], conflicts.len() as u64);
    // The conflicted path must NOT masquerade as a clean change — it used
    // to surface as a synthetic "deleted" row.
    assert!(
        !json["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "base.txt"),
        "conflict misreported as a clean change: {json}"
    );
}

#[test]
fn net_merge_conflicts_surface_in_every_output_style() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();
    let revs = ["conf-a", "conf-b", "--mode", "net-merge"];

    let stat = stdout(&oak(dir, &[&["diff"][..], &revs, &["--stat"]].concat()));
    assert!(
        stat.contains("! base.txt  conflict predicted"),
        "stat hides the conflict: {stat}"
    );
    assert!(!stat.contains("D base.txt"), "not a deletion: {stat}");

    let names = stdout(&oak(
        dir,
        &[&["diff"][..], &revs, &["--name-only"]].concat(),
    ));
    assert!(
        names.lines().any(|line| line == "base.txt"),
        "name-only hides the conflict: {names}"
    );

    let printed = stdout(&oak(dir, &[&["diff"][..], &revs, &["--print"]].concat()));
    assert!(
        printed.contains("predicted conflict: base.txt"),
        "print hides the conflict: {printed}"
    );
    assert!(
        !printed.contains("-AAA"),
        "conflict rendered as a deletion hunk: {printed}"
    );
}

#[test]
fn net_merge_conflicts_respect_path_filters() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();
    // Scoped away from base.txt: the conflict must not leak in.
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    let out = oak(
        dir,
        &[
            "diff",
            "conf-a",
            "conf-b",
            "--mode",
            "net-merge",
            "--json",
            "--",
            "docs",
        ],
    );
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert!(
        json.get("conflict_files").is_none(),
        "filtered-out conflict leaked: {json}"
    );
}

#[test]
fn exit_code_counts_predicted_conflicts_as_differences() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();
    // The only divergence between these branches is the conflicted file;
    // --exit-code must not report them as merge-identical.
    let out = oak(
        dir,
        &[
            "diff",
            "conf-a",
            "conf-b",
            "--mode",
            "net-merge",
            "--json",
            "--exit-code",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout(&out));
}

#[test]
fn double_dash_forces_revision_reading_for_ambiguous_names() {
    let (temp, _, _) = endpoint_fixture_repo();
    // `feature` is both a branch and a directory…
    std::fs::create_dir_all(temp.path().join("feature")).unwrap();
    // …but a typed `--` is the disambiguation the ambiguity error suggests,
    // so it must actually work.
    let out = oak(temp.path(), &["diff", "feature", "--"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("M base.txt"), "got: {text}");
}

#[test]
fn explicit_against_survives_into_footer_commands() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();
    let out = oak(dir, &["diff", "conf-b", "--against", "conf-a"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("oak diff conf-b --against conf-a --mode contribution --print"),
        "footer dropped --against: {text}"
    );
}

#[test]
fn unified_context_applies_to_json_hunks() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("f.txt"), "one\ntwo\nTHREE\nfour\nfive\n").unwrap();

    let out = oak(dir, &["diff", "--json", "--hunks", "-U", "0"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let patch = json["changed_files"][0]["patch"].as_str().unwrap();
    assert!(patch.contains("+THREE"), "got: {patch}");
    assert!(!patch.contains(" two"), "-U 0 must drop context: {patch}");
}

#[test]
fn word_diff_handles_content_lines_starting_with_dashes() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("cli.txt"), "--flag value one\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    // Removed line renders as `---flag value one` — prefix heuristics would
    // misread it as a file header and mispair the run.
    std::fs::write(dir.join("cli.txt"), "--flag value two\n").unwrap();

    let text = stdout(&oak(dir, &["diff", "--print", "--word-diff"]));
    assert!(text.contains("[-one-]"), "got: {text}");
    assert!(text.contains("{+two+}"), "got: {text}");
}

#[test]
fn branch_diff_renders_human_stat_table_without_json_flag() {
    let (temp, _, _) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["branch", "diff", "feature"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("M base.txt"), "got: {text}");
    // The footer points at the `oak diff` endpoint spelling, where
    // --print/--json exist.
    assert!(
        text.contains("# hunks: oak diff feature --against main --mode tree --print"),
        "got: {text}"
    );
}

#[test]
fn diff_revision_after_path_reports_ordering_not_a_false_unknown() {
    let (temp, _, _) = endpoint_fixture_repo();
    std::fs::create_dir_all(temp.path().join("docs")).unwrap();
    let out = oak(temp.path(), &["diff", "docs", "feature"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    // `feature` IS a branch — the error must name the real constraint
    // (argument order), never deny the revision exists.
    assert!(
        err.contains("revisions must come before paths"),
        "got: {err}"
    );
    assert!(
        err.contains("'feature'") && err.contains("'docs'"),
        "error should name the exact args: {err}"
    );
    assert!(
        !err.contains("is not a branch"),
        "must not deny a real branch: {err}"
    );
}

#[test]
fn diff_third_revision_reports_the_two_revision_limit() {
    let (temp, first, second) = endpoint_fixture_repo();
    let out = oak(temp.path(), &["diff", &first[..8], &second[..8], "feature"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("at most two revisions"), "got: {err}");
    assert!(err.contains("'feature'"), "got: {err}");
    assert!(
        !err.contains("is not a branch"),
        "must not deny a real branch: {err}"
    );
}

#[test]
fn external_difftool_still_surfaces_predicted_conflicts() {
    let temp = conflicting_branches_repo();
    let dir = temp.path();
    // `true` exits 0 and ignores the two tree paths — a difftool that shows
    // nothing. The predicted conflict must still reach the user afterwards.
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["diff", "conf-a", "conf-b", "--mode", "net-merge"])
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_DIFF_TOOL", "true")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("predicted conflict: base.txt"),
        "external-tool style hides the conflict: {text}"
    );
}

#[test]
fn truncated_hunks_name_a_fetch_command_per_omitted_file_with_quoting() {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("a.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
    std::fs::write(dir.join("b.txt"), "new\n").unwrap();
    std::fs::write(dir.join("with space.txt"), "spaced\n").unwrap();

    let out = oak(dir, &["diff", "--json", "--hunks", "--max-bytes", "1"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(json["hunks_truncated"], true);
    let recommended: Vec<&str> = json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command.as_str().unwrap())
        .collect();
    // Every omitted file gets its own ready-to-run fetch command, and paths
    // with shell metacharacters are quoted.
    assert!(
        recommended.contains(&"oak diff --json --hunks -- a.txt"),
        "got: {recommended:?}"
    );
    assert!(
        recommended.contains(&"oak diff --json --hunks -- b.txt"),
        "got: {recommended:?}"
    );
    assert!(
        recommended.contains(&"oak diff --json --hunks -- 'with space.txt'"),
        "got: {recommended:?}"
    );
}

#[test]
fn diff_ambiguous_revision_and_path_names_both_invocations() {
    let (temp, _, _) = endpoint_fixture_repo();
    std::fs::create_dir_all(temp.path().join("feature")).unwrap();
    let out = oak(temp.path(), &["diff", "feature"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("'feature' is both a branch and an existing path"),
        "got: {err}"
    );
    assert!(err.contains("oak diff -- feature"), "got: {err}");
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

/// Like [`fixture_repo`], but the branch's work is split across a commit and
/// the dirty tree: `src/a.txt` committed on a feature branch, `docs/x.md`
/// modified but uncommitted.
fn branch_fixture_repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("src/a.txt"), "a\nb\n").unwrap();
    std::fs::write(dir.join("docs/x.md"), "x\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", "-c", "feature"]).status.success());
    std::fs::write(dir.join("src/a.txt"), "a\nB\nc\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    std::fs::write(dir.join("docs/x.md"), "x\ny\n").unwrap();
    temp
}

#[test]
fn diff_branch_covers_commits_and_dirty_worktree() {
    let temp = branch_fixture_repo();

    // Plain diff sees only the dirty file.
    let text = stdout(&oak(temp.path(), &["diff", "--stat"]));
    assert_eq!(text, "M docs/x.md  +1 -0\n1 file changed, +1 -0\n");

    // --branch sees the committed branch work plus the dirty file, diffed
    // against the fork point — and a plain offline fork must not spray
    // caveats about main's absent local head.
    let out = oak(temp.path(), &["diff", "--branch", "--stat"]);
    let text = stdout(&out);
    assert_no_ansi("diff --branch --stat", &text);
    assert_eq!(
        text,
        "M docs/x.md  +1 -0\n\
         M src/a.txt  +2 -1\n\
         2 files changed, +3 -1\n"
    );
    assert_eq!(stderr(&out), "", "offline branch diff should not warn");

    let text = stdout(&oak(temp.path(), &["diff", "--branch", "--print"]));
    assert_no_ansi("diff --branch --print", &text);
    assert!(
        text.contains("diff --oak a/src/a.txt b/src/a.txt"),
        "committed change missing: {text}"
    );
    assert!(text.contains("+B"), "committed hunk missing: {text}");
    assert!(text.contains("+y"), "dirty hunk missing: {text}");
}

#[test]
fn diff_branch_scopes_paths_and_rejects_json() {
    let temp = branch_fixture_repo();

    let text = stdout(&oak(
        temp.path(),
        &["diff", "--branch", "--name-only", "src"],
    ));
    assert_eq!(text, "src/a.txt\n", "got: {text}");

    // --branch output has no JSON shape yet; agents get `oak branch diff
    // --json` for that, so the combination is a usage error.
    let out = oak(temp.path(), &["diff", "--branch", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage error expected, stderr: {}",
        stderr(&out)
    );
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
