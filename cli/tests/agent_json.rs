//! Machine-readable output and non-interactive safety contracts.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

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

fn fixture_repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    temp
}

fn json_stdout(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
    serde_json::from_str(&stdout(out)).expect("stdout should be valid JSON")
}

#[test]
fn status_json_contains_expected_fields_and_full_description() {
    let temp = fixture_repo();
    let dir = temp.path();
    let desc = "Subject line\nbody one\nbody two";
    assert!(oak(dir, &["desc", desc]).status.success());
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let json = json_stdout(&oak(dir, &["status", "--json"]));

    assert_eq!(json["branch_description"], desc);
    assert_eq!(json["branch_status"], "open");
    assert_eq!(json["parent"], "main");
    assert_eq!(json["unmerged_commit_count"], 1);
    assert_eq!(json["merge_in_progress"], false);
    assert_eq!(json["sync_in_progress"], false);
    assert!(json["branch"].as_str().unwrap().starts_with("tester-"));
    assert!(json["head"].as_str().unwrap().len() >= 40);

    let changes = json["changes"].as_array().unwrap();
    assert!(changes
        .iter()
        .any(|c| c["path"] == "tracked.txt" && c["status"] == "modified"));
    assert!(changes
        .iter()
        .any(|c| c["path"] == "new.txt" && c["status"] == "added"));
}

#[test]
fn log_json_is_an_array_of_commit_objects() {
    let temp = fixture_repo();

    let json = json_stdout(&oak(temp.path(), &["log", "--json"]));
    let commits = json.as_array().expect("log JSON should be an array");
    assert_eq!(commits.len(), 1);
    let commit = &commits[0];
    assert!(commit["hash"].as_str().unwrap().len() >= 40);
    assert!(commit["timestamp"].as_str().unwrap().contains('T'));
    assert!(commit["branch"].as_str().unwrap().starts_with("tester-"));
    assert_eq!(commit["description_or_subject"], "1 file changed");
    assert_eq!(commit["files_changed"], 1);
}

#[test]
fn branch_list_json_defaults_from_bare_branch_command() {
    let temp = fixture_repo();

    let json = json_stdout(&oak(temp.path(), &["branch", "list", "--json"]));
    let branches = json
        .as_array()
        .expect("branch list JSON should be an array");
    assert_eq!(branches.len(), 1);
    let branch = &branches[0];
    assert!(branch["name"].as_str().unwrap().starts_with("tester-"));
    assert!(branch["head"].as_str().unwrap().len() >= 40);
    assert_eq!(branch["description"], Value::Null);
    assert_eq!(branch["status"], "open");
    assert_eq!(branch["current"], true);
    assert!(branch["created_at"].as_str().unwrap().contains('T'));

    let bare = json_stdout(&oak(temp.path(), &["branch", "--json"]));
    assert_eq!(bare, json);
}

#[test]
fn reset_refuses_non_interactive_discard_with_dirty_exit_code() {
    let temp = fixture_repo();
    let dir = temp.path();
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

    let out = oak(dir, &["reset"]);

    assert_eq!(out.status.code(), Some(4));
    assert!(
        stderr(&out).contains(
            "refusing to discard 1 change(s) without --force when not running interactively"
        ),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
        "changed\n"
    );
}

#[test]
fn held_workdir_lock_exits_with_retryable_lock_code() {
    let temp = fixture_repo();
    std::fs::write(
        temp.path().join(".oak/wdlock"),
        std::process::id().to_string(),
    )
    .unwrap();

    let out = oak(temp.path(), &["commit"]);

    assert_eq!(out.status.code(), Some(3));
    assert!(
        stderr(&out).contains("Repository is locked by another process"),
        "stderr: {}",
        stderr(&out)
    );
}

/// A filename the tree format cannot represent (`\n` is a tree-encoding
/// delimiter) is a usage error, not a generic failure: exit code 2 so an
/// unattended agent can tell "fix your input" from "something broke".
#[test]
fn hostile_filename_commit_exits_with_usage_code() {
    let temp = fixture_repo();
    std::fs::write(temp.path().join("a\nb.txt"), "x\n").unwrap();

    let out = oak(temp.path(), &["commit"]);
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(2),
        "InvalidPath should map to the usage exit code\nstderr:\n{}",
        stderr(&out)
    );
}
