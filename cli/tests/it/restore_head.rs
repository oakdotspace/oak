//! Literal HEAD is the same restore source as omitting --source.
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn oak(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(root)
        .env("OAK_AUTHOR", "restore-head-test")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> Output {
    let out = oak(root, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    out
}

fn identity(root: &Path) -> serde_json::Value {
    let out = success(root, &["status", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    serde_json::json!({"head": value["head"], "branch": value["branch"]})
}

#[test]
fn explicit_head_restores_selected_path_without_committing_or_touching_neighbors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    success(root, &["init"]);
    fs::write(root.join("target.txt"), b"checkpoint\n").unwrap();
    fs::write(root.join("neighbor.txt"), b"neighbor\n").unwrap();
    success(root, &["commit"]);
    let before = identity(root);
    fs::write(root.join("target.txt"), b"discard me\n").unwrap();
    fs::write(root.join("neighbor.txt"), b"keep dirty\n").unwrap();
    fs::write(root.join("untracked.txt"), b"keep untracked\n").unwrap();
    success(
        root,
        &["restore", "--source", "HEAD", "--force", "--", "target.txt"],
    );
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"checkpoint\n");
    assert_eq!(
        fs::read(root.join("neighbor.txt")).unwrap(),
        b"keep dirty\n"
    );
    assert_eq!(
        fs::read(root.join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    assert_eq!(identity(root), before);
}

#[test]
fn explicit_head_refuses_noninteractive_discard_without_force() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    success(root, &["init"]);
    fs::write(root.join("target.txt"), b"checkpoint\n").unwrap();
    success(root, &["commit"]);
    fs::write(root.join("target.txt"), b"keep dirty\n").unwrap();
    fs::write(root.join("untracked.txt"), b"keep untracked\n").unwrap();
    let before = identity(root);
    let out = oak(root, &["restore", "--source", "HEAD"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without --force"));
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"keep dirty\n");
    assert_eq!(
        fs::read(root.join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    assert_eq!(identity(root), before);
}

#[test]
fn explicit_head_matches_implicit_unborn_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    success(root, &["init"]);
    fs::write(root.join("untracked.txt"), b"keep\n").unwrap();
    let before = identity(root);
    let implicit = success(root, &["restore", "--force"]);
    let explicit = success(root, &["restore", "--source", "HEAD", "--force"]);
    assert_eq!(explicit.stdout, implicit.stdout);
    assert_eq!(explicit.stderr, implicit.stderr);
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("No commits yet"));
    assert_eq!(fs::read(root.join("untracked.txt")).unwrap(), b"keep\n");
    assert_eq!(identity(root), before);
}

#[test]
fn historical_hash_still_restores_old_bytes_without_moving_head() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    success(root, &["init"]);
    fs::write(root.join("target.txt"), b"old\n").unwrap();
    success(root, &["commit"]);
    let old = identity(root)["head"].as_str().unwrap().to_owned();
    fs::write(root.join("target.txt"), b"current\n").unwrap();
    success(root, &["commit"]);
    let before = identity(root);
    success(root, &["restore", "--source", &old, "--force"]);
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"old\n");
    assert_eq!(identity(root), before);
    success(root, &["restore", "--source", "HEAD", "--force"]);
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"current\n");
    assert_eq!(identity(root), before);
}

#[test]
fn explicit_head_keeps_ignored_tracked_path_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    success(root, &["init"]);
    fs::write(root.join("debug.log"), b"tracked log\n").unwrap();
    success(root, &["commit"]);
    fs::write(root.join(".oakignore"), b"*.log\n").unwrap();
    let before = identity(root);
    let implicit = success(root, &["restore", "--force", "--", "debug.log"]);
    let explicit = success(
        root,
        &["restore", "--source", "HEAD", "--force", "--", "debug.log"],
    );
    assert_eq!(explicit.stdout, implicit.stdout);
    assert_eq!(explicit.stderr, implicit.stderr);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&explicit.stdout),
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert!(combined.contains("ignore rules") && combined.contains("debug.log"));
    assert!(!combined.contains("already at source state"));
    assert_eq!(fs::read(root.join("debug.log")).unwrap(), b"tracked log\n");
    assert_eq!(identity(root), before);
}
