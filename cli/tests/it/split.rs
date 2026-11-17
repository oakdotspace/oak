//! `oak split --plan` end to end: the scriptable front end agents drive.
//!
//! A split rewrites history, so the contract is mostly about what it must
//! *not* do: `--dry-run` writes nothing, and every rejected plan (malformed,
//! incomplete, dependent segments, taken branch names) leaves every ref and
//! the working tree exactly as they were.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn oak(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oak binary should spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().expect("oak binary should run")
}

fn ok(dir: &Path, args: &[&str]) -> Output {
    let out = oak(dir, args, "");
    assert!(
        out.status.success(),
        "oak {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn status(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&ok(dir, &["status", "--json"]).stdout).unwrap()
}

fn branches(dir: &Path) -> Vec<String> {
    let out = ok(dir, &["branch", "list", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = json
        .as_array()
        .or_else(|| json["branches"].as_array())
        .expect("branch list rows");
    let mut names: Vec<String> = rows
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

/// A branch whose three commits each add one independent file, plus the
/// branch name and the commit hashes (oldest first).
struct Fixture {
    temp: tempfile::TempDir,
    branch: String,
    commits: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        ok(dir, &["init", "."]);
        let mut commits = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), format!("{name}\n")).unwrap();
            ok(dir, &["commit"]);
            commits.push(status(dir)["head"].as_str().unwrap().to_string());
        }
        let branch = status(dir)["branch"].as_str().unwrap().to_string();
        Fixture {
            temp,
            branch,
            commits,
        }
    }

    fn dir(&self) -> &Path {
        self.temp.path()
    }

    fn split(&self, plan: &str, extra: &[&str]) -> Output {
        let mut args = vec!["split", "--plan", "-"];
        args.extend_from_slice(extra);
        oak(self.dir(), &args, plan)
    }

    fn head(&self) -> String {
        status(self.dir())["head"].as_str().unwrap().to_string()
    }

    fn exists(&self, path: &str) -> bool {
        self.dir().join(path).exists()
    }

    /// Everything a rejected plan must leave alone.
    fn assert_untouched(&self, before: &[String]) {
        assert_eq!(self.head(), self.commits[2], "HEAD moved");
        assert_eq!(&branches(self.dir()), before, "branch set changed");
        assert!(self.exists("a.txt") && self.exists("b.txt") && self.exists("c.txt"));
        assert_eq!(status(self.dir())["changes"], serde_json::json!([]));
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn dry_run_describes_the_split_and_writes_nothing() {
    let fx = Fixture::new();
    let before = branches(fx.dir());
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];
    let plan = format!("pick {a}\n# keep b separate\nsplit side\npick {b}\ndrop {c}\n");

    let out = fx.split(&plan, &["--dry-run"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(
        text.contains(&format!("{} (rewritten)", fx.branch)),
        "{text}"
    );
    assert!(text.contains("new branch off main: side"), "{text}");
    assert!(text.contains("dry run"), "{text}");
    assert!(
        !text.contains(&format!("pick {}", &c[..8])),
        "dropped commit listed: {text}"
    );
    fx.assert_untouched(&before);
}

#[test]
fn split_and_drop_rewrite_the_branch_and_working_tree() {
    let fx = Fixture::new();
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];
    // Short prefixes and the one-letter keywords are accepted.
    let plan = format!("p {}\ns side\np {}\nd {}\n", &a[..12], &b[..12], &c[..12]);

    let out = fx.split(&plan, &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let mut expected = vec![fx.branch.clone(), "side".to_string()];
    expected.sort();
    assert_eq!(branches(fx.dir()), expected);
    // The source branch keeps only `a` (replayed onto the same base, so the
    // same commit), and the tree follows it.
    assert_eq!(&fx.head(), a);
    assert!(fx.exists("a.txt"));
    assert!(!fx.exists("b.txt") && !fx.exists("c.txt"));
    assert_eq!(status(fx.dir())["changes"], serde_json::json!([]));

    // `side` holds only `b`, rebuilt off the original base.
    ok(fx.dir(), &["switch", "side"]);
    assert!(fx.exists("b.txt"));
    assert!(!fx.exists("a.txt") && !fx.exists("c.txt"));
}

#[test]
fn reordering_independent_commits_keeps_every_file() {
    let fx = Fixture::new();
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];

    let out = fx.split(&format!("pick {c}\npick {a}\npick {b}\n"), &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_ne!(&fx.head(), c);
    assert!(fx.exists("a.txt") && fx.exists("b.txt") && fx.exists("c.txt"));
    assert_eq!(status(fx.dir())["changes"], serde_json::json!([]));
}

#[test]
fn dependent_segment_conflicts_and_changes_nothing() {
    let fx = Fixture::new();
    // A fourth commit edits a file the first commit introduced.
    std::fs::write(fx.dir().join("a.txt"), "edited\n").unwrap();
    ok(fx.dir(), &["commit"]);
    let d = fx.head();
    let before = branches(fx.dir());
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];

    let out = fx.split(
        &format!("pick {a}\npick {b}\npick {c}\nsplit side\npick {d}\n"),
        &[],
    );

    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("can't be built independently"), "{err}");
    assert!(err.contains("a.txt"), "names the conflicting path: {err}");
    assert_eq!(fx.head(), d);
    assert_eq!(branches(fx.dir()), before);
    assert_eq!(
        std::fs::read_to_string(fx.dir().join("a.txt")).unwrap(),
        "edited\n"
    );
}

#[test]
fn invalid_plans_are_refused_before_anything_moves() {
    let fx = Fixture::new();
    let before = branches(fx.dir());
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];
    let cases: Vec<(String, &str)> = vec![
        (format!("pick {a}\npick {b}\n"), "missing from the plan"),
        (
            format!("pick {a}\npick {a}\npick {b}\npick {c}\n"),
            "more than once",
        ),
        (
            format!("pick {a}\ndrop {a}\npick {b}\npick {c}\n"),
            "more than once",
        ),
        (
            format!("drop {a}\ndrop {b}\ndrop {c}\n"),
            "drops every commit",
        ),
        (format!("squash {a}\n"), "unknown plan keyword 'squash'"),
        ("pick\n".to_string(), "line 1: `pick` needs a commit"),
        (format!("pick {a}\ndrop\n"), "line 2: `drop` needs a commit"),
        (
            format!("pick {a}\nsplit\n"),
            "line 2: `split` needs a branch name",
        ),
        (
            "pick ffffffffffff\n".to_string(),
            "no commit on this branch matches",
        ),
        (
            format!("pick {a}\nsplit main\npick {b}\npick {c}\n"),
            "can't be named 'main'",
        ),
        (
            format!("pick {a}\nsplit {}\npick {b}\npick {c}\n", fx.branch),
            "already exists",
        ),
        (
            format!("pick {a}\npick {b}\npick {c}\nsplit empty\n"),
            "would have no commits",
        ),
    ];

    for (plan, expected) in cases {
        let out = fx.split(&plan, &[]);
        assert!(!out.status.success(), "plan should fail:\n{plan}");
        let err = stderr(&out);
        assert!(
            err.contains(expected),
            "plan:\n{plan}\nexpected {expected:?} in:\n{err}"
        );
        fx.assert_untouched(&before);
    }
}

#[test]
fn split_target_that_already_exists_is_refused() {
    let fx = Fixture::new();
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];
    ok(fx.dir(), &["switch", "-c", "taken"]);
    ok(fx.dir(), &["switch", &fx.branch]);
    let before = branches(fx.dir());

    let out = fx.split(&format!("pick {a}\nsplit taken\npick {b}\npick {c}\n"), &[]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("taken"), "{}", stderr(&out));
    fx.assert_untouched(&before);
}

#[test]
fn main_has_no_editable_history() {
    let fx = Fixture::new();
    let out = fx.split("", &["--from", "main"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("'main' has no editable local history"));
}

#[test]
fn conflict_in_a_later_split_creates_no_earlier_split_branch() {
    let fx = Fixture::new();
    std::fs::write(fx.dir().join("a.txt"), "edited\n").unwrap();
    ok(fx.dir(), &["commit"]);
    let d = fx.head();
    let before = branches(fx.dir());
    let [a, b, c] = [&fx.commits[0], &fx.commits[1], &fx.commits[2]];

    let out = fx.split(
        &format!("pick {a}\npick {c}\nsplit one\npick {b}\nsplit two\npick {d}\n"),
        &[],
    );

    assert!(!out.status.success());
    assert!(stderr(&out).contains("branch 'two'"), "{}", stderr(&out));
    assert_eq!(branches(fx.dir()), before, "'one' must not be created");
    assert_eq!(fx.head(), d);
}
