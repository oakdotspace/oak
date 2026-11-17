//! Local commit hooks (`.oak/hooks/pre-commit`, `.oak/hooks/post-commit`).
//!
//! These run the real `oak` binary so the hook is spawned exactly as a user's
//! would be. The contract under test: a failing `pre-commit` aborts before any
//! repository state moves, `--no-verify` skips both hooks, a failing
//! `post-commit` only warns, and hook output never lands on stdout, where it
//! would corrupt `--json` documents.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
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

/// A repo with one commit, so later commits have a HEAD to compare against.
fn repo() -> tempfile::TempDir {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    temp
}

fn install_hook(dir: &Path, event: &str, script: &str, executable: bool) {
    let hooks = dir.join(".oak").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join(event);
    std::fs::write(&path, script).unwrap();
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn head(dir: &Path) -> String {
    let out = oak(dir, &["status", "--json"]);
    assert!(out.status.success(), "status failed: {}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    json["head"].as_str().unwrap().to_string()
}

fn changed_paths(dir: &Path) -> Vec<String> {
    let out = oak(dir, &["status", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    json["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn failing_pre_commit_aborts_without_moving_head() {
    let temp = repo();
    let dir = temp.path();
    let before = head(dir);
    install_hook(
        dir,
        "pre-commit",
        "#!/bin/sh\necho 'lint failed' >&2\nexit 3\n",
        true,
    );
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit"]);

    assert!(!out.status.success(), "a failing pre-commit must abort");
    let err = stderr(&out);
    assert!(err.contains("lint failed"), "hook output is shown:\n{err}");
    assert!(err.contains("exit 3"), "names the hook's exit code:\n{err}");
    assert!(err.contains("--no-verify"), "names the bypass:\n{err}");
    assert_eq!(head(dir), before, "HEAD must not move");
    assert_eq!(
        changed_paths(dir),
        vec!["new.txt"],
        "work stays uncommitted"
    );
}

#[test]
fn no_verify_skips_a_failing_pre_commit() {
    let temp = repo();
    let dir = temp.path();
    let before = head(dir);
    install_hook(dir, "pre-commit", "#!/bin/sh\nexit 1\n", true);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit", "--no-verify"]);

    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    assert!(!stderr(&out).contains("Running pre-commit hook"));
    assert_ne!(head(dir), before);
    assert!(changed_paths(dir).is_empty());
}

#[test]
fn pre_commit_runs_at_work_tree_root_and_its_edits_are_committed() {
    let temp = repo();
    let dir = temp.path();
    // The hook records its environment and rewrites a tracked file, like a
    // formatter would. Run from a subdirectory to prove cwd is the root.
    install_hook(
        dir,
        "pre-commit",
        "#!/bin/sh\npwd -P > hook-cwd.txt\necho \"$OAK_HOOK\" > hook-event.txt\necho formatted > new.txt\n",
        true,
    );
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("new.txt"), "unformatted\n").unwrap();

    let out = oak(&dir.join("sub"), &["commit"]);

    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    let root = dir.canonicalize().unwrap();
    let cwd = std::fs::read_to_string(dir.join("hook-cwd.txt")).unwrap();
    assert_eq!(Path::new(cwd.trim()), root);
    let event = std::fs::read_to_string(dir.join("hook-event.txt")).unwrap();
    assert_eq!(event.trim(), "pre-commit");
    // A clean tree holding the hook's rewrite means HEAD has the rewrite too.
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "formatted\n"
    );
    assert!(
        changed_paths(dir).is_empty(),
        "files the hook wrote must be part of the commit"
    );
}

#[test]
fn non_executable_hook_is_ignored() {
    let temp = repo();
    let dir = temp.path();
    let before = head(dir);
    install_hook(dir, "pre-commit", "#!/bin/sh\nexit 1\n", false);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit"]);

    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    assert_ne!(head(dir), before);
}

#[test]
fn failing_post_commit_only_warns() {
    let temp = repo();
    let dir = temp.path();
    let before = head(dir);
    install_hook(dir, "post-commit", "#!/bin/sh\nexit 7\n", true);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit"]);

    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    assert!(stderr(&out).contains("post-commit hook failed (exit 7)"));
    assert_ne!(head(dir), before, "the commit already landed");
}

#[test]
fn hook_stdout_never_corrupts_commit_json() {
    let temp = repo();
    let dir = temp.path();
    // The `cargo` template echoes progress on stdout; so do most hooks.
    install_hook(
        dir,
        "pre-commit",
        "#!/bin/sh\necho 'pre-commit: cargo fmt --all'\n",
        true,
    );
    install_hook(dir, "post-commit", "#!/bin/sh\necho 'post done'\n", true);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit", "--json"]);

    assert!(out.status.success(), "stderr:\n{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON document ({e}):\n{}", stdout(&out)));
    assert_eq!(json["committed"], true);
    let err = stderr(&out);
    assert!(err.contains("pre-commit: cargo fmt --all"), "{err}");
    assert!(err.contains("post done"), "{err}");
}

#[test]
fn failing_pre_commit_json_emits_only_the_error_envelope() {
    let temp = repo();
    let dir = temp.path();
    install_hook(dir, "pre-commit", "#!/bin/sh\necho noisy\nexit 2\n", true);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    let out = oak(dir, &["commit", "--json"]);

    assert!(!out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON document ({e}):\n{}", stdout(&out)));
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("pre-commit hook failed (exit 2)"),
        "{message}"
    );
}
