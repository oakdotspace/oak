//! Remote head observation (F3) checks that need no server. The tests
//! against a real server (fb-627 branch creation, fb-629 dry-run
//! verification) run
//! separately.
//!
//! The last test is the mechanical guard: the persisted observation and the
//! freshness window live only in `cli/src/remote_head.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use oak_core::{Repository, SqliteRepository};
use serde_json::Value;

fn oak(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", "remote-head-token")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_WATCH", "0")
        .env("OAK_AUTHOR", "tester")
        .args(args)
        .output()
        .unwrap()
}

fn ok(output: Output, label: &str) -> Output {
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn db(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn local_head(dir: &Path, branch: &str) -> Option<String> {
    db(dir)
        .get_branch_head(branch)
        .unwrap()
        .map(|head| head.to_string())
}

/// A repo whose `main` has no commits seeds the new branch at the current
/// head; the receipt names that commit instead of `base_head: null`.
#[test]
fn start_on_an_empty_main_reports_the_commit_the_branch_starts_from() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    ok(oak(&home, &repo, &["init", "."]), "init");
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    ok(oak(&home, &repo, &["commit"]), "commit");
    let store = db(&repo);
    let current = store.get_current_branch_name().unwrap().unwrap();
    let head = store
        .get_branch_head(&current)
        .unwrap()
        .unwrap()
        .to_string();
    drop(store);

    let receipt = json(&ok(
        oak(
            &home,
            &repo,
            &["start", "next", "--name", "next", "--no-watch", "--json"],
        ),
        "start",
    ));
    assert_eq!(local_head(&repo, "next").as_deref(), Some(head.as_str()));
    assert_eq!(receipt["base_head"], head.as_str(), "{receipt}");
    assert_eq!(receipt["base_branch"], current.as_str());
    assert_eq!(receipt["base_head_source"], "local_only");
    assert!(receipt["base_head_observed_at"].is_null());
}

/// `--offline` only means something for the dry-run.
#[test]
fn offline_requires_dry_run() {
    let temp = tempfile::tempdir().unwrap();
    let out = oak(temp.path(), temp.path(), &["merge", "--offline", "--json"]);
    assert_eq!(out.status.code(), Some(2));
}

/// Mechanical guard (DESIGN principle 6): the persisted observation and the
/// freshness window have exactly one home. A second reader of the raw
/// metadata, or a second TTL, is how fb-117/fb-627/fb-629 happened.
#[test]
fn the_remote_head_record_and_ttl_live_only_in_remote_head_rs() {
    fn visit(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    visit(&src, &mut files);
    assert!(files.len() > 50, "scanned {} files", files.len());

    let home = src.join("remote_head.rs");
    let mut offenders = Vec::new();
    for file in &files {
        if *file == home {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            if code.contains("MetadataKey::MainLastCheckedAt")
                || code.contains("MetadataKey::MainLastObservedHead")
                || code.contains("FRESHNESS_TTL_SECS")
                || code.contains("REFRESH_CACHE_TTL_SECS")
            {
                offenders.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(&src).unwrap().display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "remote-head freshness must go through cli/src/remote_head.rs \
         (observe / on_record / record_verified):\n{}",
        offenders.join("\n")
    );
    let home_text = std::fs::read_to_string(&home).unwrap();
    assert_eq!(
        home_text.matches("_TTL_SECS: u64 =").count(),
        1,
        "exactly one remote-head TTL constant"
    );
}
