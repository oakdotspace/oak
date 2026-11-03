//! End-to-end tests for git-backed repositories hosted on a plain git remote.
//!
//! A local bare repository stands in for GitHub: `oak init --git`, `oak
//! commit`, `oak push`, `oak clone <url> --git`, `oak pull`, and `oak finish`
//! must round-trip real git objects and refs through it, and `git status`
//! must agree with `oak status` after every Oak operation.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_AUTHOR", "tester")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("oak should run")
}

fn oak_ok(dir: &Path, args: &[&str]) -> String {
    let out = oak(dir, args);
    assert!(
        out.status.success(),
        "oak {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn git_backed_repo_round_trips_through_a_git_host() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let hub = root.join("hub.git");
    git(
        root,
        &["init", "-q", "--bare", "-b", "main", hub.to_str().unwrap()],
    );

    // Author A: a fresh git-backed repo, first commit straight onto main.
    oak_ok(root, &["init", "--git", "a"]);
    let a = root.join("a");
    git(&a, &["remote", "add", "origin", hub.to_str().unwrap()]);
    fs::write(a.join("README.md"), "line1\nline2\n").unwrap();
    oak_ok(&a, &["commit"]);
    assert_eq!(oak_ok(&a, &["status"]).trim(), "");
    assert_eq!(
        git(&a, &["status", "--porcelain"]),
        "",
        "git index must follow oak commits"
    );
    assert_eq!(
        git(&a, &["log", "-1", "--format=%an <%ae>"]),
        "Test Author <author@example.com>"
    );
    oak_ok(&a, &["push"]);
    assert_eq!(
        git(&hub, &["rev-parse", "main"]),
        git(&a, &["rev-parse", "HEAD"])
    );

    // Author B: a git-backed clone is clean and works on a feature branch
    // whose description becomes the git commit message.
    oak_ok(root, &["clone", "--git", hub.to_str().unwrap(), "b"]);
    let b = root.join("b");
    assert_eq!(
        oak_ok(&b, &["status"]).trim(),
        "",
        "fresh clone must be clean"
    );
    oak_ok(&b, &["switch", "-c", "feat"]);
    oak_ok(&b, &["desc", "Introduce the b module"]);
    fs::write(b.join("b.txt"), "b\n").unwrap();
    oak_ok(&b, &["commit"]);
    // The commit message describes the checkpoint itself, with the branch
    // description as context and machine-readable trailers.
    assert_eq!(git(&b, &["log", "-1", "--format=%s"]), "Add b.txt");
    let body = git(&b, &["log", "-1", "--format=%b"]);
    assert!(
        body.contains("Checkpoint 1 on feat (parent: main): 1 file changed, +1 -0"),
        "{body}"
    );
    assert!(body.contains("  A  b.txt  +1"), "{body}");
    assert!(
        body.contains("Branch description:\n  Introduce the b module"),
        "{body}"
    );
    assert_eq!(
        git(
            &b,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Oak-Checkpoint,valueonly)"
            ]
        ),
        "1"
    );
    let pushed: serde_json::Value = serde_json::from_str(&oak_ok(&b, &["push", "--json"])).unwrap();
    assert_eq!(pushed["backend"], "git");
    assert_eq!(pushed["branch"], "feat");
    assert_eq!(
        pushed["pushed_head"].as_str().unwrap(),
        git(&hub, &["rev-parse", "feat"])
    );

    // Main moves on; `oak pull` on the feature branch merges it in.
    fs::write(a.join("a.txt"), "a\n").unwrap();
    oak_ok(&a, &["commit"]);
    oak_ok(&a, &["push"]);
    let pulled: serde_json::Value = serde_json::from_str(&oak_ok(&b, &["pull", "--json"])).unwrap();
    assert_eq!(pulled["conflict_paths"].as_array().unwrap().len(), 0);
    assert!(b.join("a.txt").exists() && b.join("b.txt").exists());
    assert_eq!(oak_ok(&b, &["status"]).trim(), "");

    // `oak finish` checkpoints dirty work under the description and pushes.
    fs::write(b.join("b.txt"), "b2\n").unwrap();
    let desc = root.join("desc.txt");
    fs::write(&desc, "Introduce the b module\n\nWith a second line.\n").unwrap();
    let finished: serde_json::Value = serde_json::from_str(&oak_ok(
        &b,
        &["finish", "--desc-file", desc.to_str().unwrap(), "--json"],
    ))
    .unwrap();
    assert_eq!(finished["committed"], true);
    assert_eq!(finished["pushed"], true);
    assert_eq!(
        git(&hub, &["rev-parse", "feat"]),
        git(&b, &["rev-parse", "HEAD"])
    );
    // Numbering continues past `oak pull`'s merge commit.
    assert_eq!(git(&b, &["log", "-1", "--format=%s"]), "Update b.txt");
    assert_eq!(
        git(
            &b,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Oak-Checkpoint,valueonly)"
            ]
        ),
        "2"
    );

    // Server-side merge has no git equivalent; it points at the host instead.
    let merge = oak(&b, &["merge"]);
    assert!(!merge.status.success());
    assert!(String::from_utf8_lossy(&merge.stderr).contains("git-backed"));

    // In a git repo `main` is a real local branch.
    oak_ok(&b, &["switch", "main"]);
    assert!(!b.join("b.txt").exists());
}

#[test]
fn pull_conflict_leaves_git_merge_state_and_reports_paths() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let hub = root.join("hub.git");
    git(
        root,
        &["init", "-q", "--bare", "-b", "main", hub.to_str().unwrap()],
    );

    oak_ok(root, &["init", "--git", "a"]);
    let a = root.join("a");
    git(&a, &["remote", "add", "origin", hub.to_str().unwrap()]);
    fs::write(a.join("f.txt"), "base\n").unwrap();
    oak_ok(&a, &["commit"]);
    oak_ok(&a, &["push"]);

    oak_ok(root, &["clone", "--git", hub.to_str().unwrap(), "b"]);
    let b = root.join("b");
    oak_ok(&b, &["switch", "-c", "feat"]);
    fs::write(b.join("f.txt"), "from b\n").unwrap();
    oak_ok(&b, &["commit"]);

    fs::write(a.join("f.txt"), "from a\n").unwrap();
    oak_ok(&a, &["commit"]);
    oak_ok(&a, &["push"]);

    let out = oak(&b, &["pull", "--json"]);
    assert!(!out.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["conflict_paths"], serde_json::json!(["f.txt"]));
    assert!(b.join(".git/MERGE_HEAD").exists());
}
