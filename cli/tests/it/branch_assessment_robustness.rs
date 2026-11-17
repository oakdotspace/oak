//! Branch assessment robustness (fb-118, fb-120, fb-327).
//!
//! - fb-118: one stored branch row with an undecodable `created_at` must not
//!   abort `oak branch triage` or unrelated `oak branch review`; the bad row
//!   becomes a typed per-branch error and every other branch is assessed.
//! - fb-120/fb-327, after Oak CI was removed: `oak branch review --json` and
//!   `oak branch triage --json` keep their `checks` object (not required,
//!   reason `ci_removed`), and `merge_allowed` rests on the VCS gates alone.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, Repository, SqliteRepository};
use serde_json::Value;

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn json_stdout(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout should be valid JSON")
}

fn db(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

/// A repo whose local `main` equals the first commit, plus a clean
/// contributing branch `feature-clean` and a second branch `other`. Both
/// parent onto `main`, so a local review is uncertified (fb-629).
fn fixture() -> (tempfile::TempDir, String) {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    {
        let repo = db(dir);
        let branch = repo.get_current_branch_name().unwrap().unwrap();
        let head = repo.get_branch_head(&branch).unwrap().unwrap();
        if repo.get_branch("main").unwrap().is_none() {
            repo.store_branch(&Branch::new("main".into(), None, None))
                .unwrap();
        }
        repo.set_branch_head("main", &head).unwrap();
    }
    assert!(oak(dir, &["switch", "-c", "other"]).status.success());
    std::fs::write(dir.join("other.txt"), "other\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    assert!(oak(dir, &["switch", "-c", "feature-clean"])
        .status
        .success());
    std::fs::write(dir.join("tracked.txt"), "branch contribution\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let repo = db(dir);
    let head = repo
        .get_branch_head("feature-clean")
        .unwrap()
        .unwrap()
        .to_string();
    (temp, head)
}

fn corrupt_created_at(dir: &Path, branch: &str, value: &str) {
    let conn = rusqlite::Connection::open(dir.join(".oak/oak.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE branches SET created_at = ?1 WHERE name = ?2",
            rusqlite::params![value, branch],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

fn review(dir: &Path, branch: &str) -> Value {
    json_stdout(&oak(
        dir,
        &["branch", "review", branch, "--merge-preview", "--json"],
    ))
}

fn triage_row<'a>(triage: &'a Value, branch: &str) -> &'a Value {
    triage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == branch)
        .unwrap_or_else(|| panic!("no triage row for {branch}: {triage}"))
}

// --- fb-118 --------------------------------------------------------------

#[test]
fn corrupt_created_at_row_is_isolated_in_triage_and_review() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    corrupt_created_at(dir, "other", "not-a-timestamp");

    let triage = json_stdout(&oak(dir, &["branch", "triage", "--json"]));
    let bad = triage_row(&triage, "other");
    assert_eq!(bad["error"]["code"], "branch_metadata_unreadable");
    assert_eq!(bad["error"]["branch"], "other");
    assert_eq!(bad["error"]["column"], "created_at");
    assert_eq!(bad["reason"], "branch_metadata_unreadable");
    assert_eq!(bad["merge_allowed"], false);
    let rendered = triage.to_string();
    assert!(!rendered.contains("premature end of input"), "{rendered}");
    assert!(!rendered.contains("not-a-timestamp"), "{rendered}");

    let good = triage_row(&triage, "feature-clean");
    assert!(good.get("error").is_none(), "{good}");
    assert_eq!(good["contribution"], "contributes", "{good}");

    // Unrelated single-branch review is unaffected by the corrupt row.
    let single = review(dir, "feature-clean");
    assert!(single.get("error").is_none(), "{single}");
    assert_eq!(single["contribution"], "contributes", "{single}");
}

#[test]
fn legacy_sqlite_default_created_at_is_read_normally() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    // The `datetime('now')` column-default form that used to abort every
    // listing with "Database error: premature end of input".
    corrupt_created_at(dir, "other", "2026-01-02 03:04:05");

    let triage = json_stdout(&oak(dir, &["branch", "triage", "--json"]));
    assert!(triage_row(&triage, "other").get("error").is_none());
    let list = json_stdout(&oak(dir, &["branch", "list", "--json"]));
    let other = list
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["name"] == "other")
        .unwrap();
    assert_eq!(other["created_at"], "2026-01-02T03:04:05+00:00");
    assert!(oak(dir, &["status", "--json"]).status.success());
}

// --- fb-120 / fb-327, after Oak CI ----------------------------------------

#[test]
fn checks_say_ci_was_removed_and_merge_rests_on_the_vcs_gates() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    let json = review(dir, "feature-clean");
    let checks = &json["checks"];
    assert_eq!(checks["state"], "not_queried", "{json}");
    assert_eq!(checks["reason"], "ci_removed");
    assert_eq!(checks["required"], false);
    assert_eq!(checks["known_passed"], false);
    assert!(checks["source"].is_null());
    assert_eq!(json["merge_allowed"], json["vcs_merge_safe"], "{json}");

    let triage = json_stdout(&oak(dir, &["branch", "triage", "--json"]));
    let row = triage_row(&triage, "feature-clean");
    assert_eq!(row["checks"]["reason"], "ci_removed", "{row}");
}

#[test]
fn dirty_current_worktree_withholds_merge_allowed() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    std::fs::write(dir.join("uncommitted.txt"), "dirty\n").unwrap();
    let json = review(dir, "feature-clean");
    assert_eq!(json["merge_allowed"], false, "{json}");
}
