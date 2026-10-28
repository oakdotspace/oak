//! fb-168(b) / fb-324: `oak space inventory --verify-local [--include-ci]`
//! fills working-tree and unpublished-commit facts per checkout under that
//! checkout's workdir lock, reports `unverified` when the lock is held, and
//! adds exact-head CI evidence from one bounded listing per repository.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", root.join(".home"))
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_MOUNTS_ROOT", root.join("absent-test-mount-registry"))
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn ok(out: Output) -> Output {
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A fresh checkout at `root/<name>` with one commit on its branch.
fn checkout(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    ok(oak(root, &dir, &["init", "."]));
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    ok(oak(root, &dir, &["commit"]));
    dir
}

fn inventory(root: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["space", "inventory", "--json", "--verify-local"];
    args.extend_from_slice(extra);
    let out = ok(oak(root, root, &args));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn entry<'a>(value: &'a Value, suffix: &str) -> &'a Value {
    value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["root"].as_str().unwrap().ends_with(suffix))
        .unwrap_or_else(|| panic!("no entry for {suffix}: {value}"))
}

fn tempdir() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".home")).unwrap();
    temp
}

#[test]
fn verify_local_reports_clean_dirty_and_unpublished_without_writing_the_database() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let clean = checkout(&root, "clean");
    let dirty = checkout(&root, "dirty");
    std::fs::write(dirty.join("a.txt"), "changed\n").unwrap();
    std::fs::write(dirty.join("new.txt"), "new\n").unwrap();
    // Status first so both stat caches are warm, then pin the db bytes.
    ok(oak(&root, &clean, &["status"]));
    ok(oak(&root, &dirty, &["status"]));
    let db_before = std::fs::read(dirty.join(".oak/oak.db")).unwrap();

    let value = inventory(&root, &[]);
    assert_eq!(
        value["coverage_scope"],
        "directory_discovery_with_locked_local_verification"
    );
    let c = entry(&value, "/clean");
    assert_eq!(c["local_verification"], "verified");
    assert_eq!(c["working_tree"], "clean");
    assert_eq!(c["working_tree_change_count"], 0);
    // Never linked: every commit on the branch is local-only.
    assert_eq!(c["unpublished_commits"], "present");
    assert_eq!(c["unpublished_commit_count"], 1);
    assert_eq!(c["unpublished_commits_basis"], "no_remote_configured");
    assert_eq!(
        c["unpublished_by_branch"][0]["basis"],
        "no_remote_configured"
    );
    assert_eq!(c["remote_publication"], "unverified");
    assert!(c.get("safe_to_delete").is_none());

    let d = entry(&value, "/dirty");
    assert_eq!(d["working_tree"], "dirty");
    assert_eq!(d["working_tree_change_count"], 2);

    // The same count `oak status --json` reports.
    let status: Value =
        serde_json::from_slice(&ok(oak(&root, &dirty, &["status", "--json"])).stdout).unwrap();
    assert_eq!(status["changes"].as_array().unwrap().len(), 2);

    // Read-only: database bytes unchanged and the inspection lock released.
    assert_eq!(db_before, std::fs::read(dirty.join(".oak/oak.db")).unwrap());
    assert!(!dirty.join(".oak/wdlock").exists());
    assert!(!clean.join(".oak/wdlock").exists());
}

#[test]
fn verify_local_reports_unverified_when_the_checkout_lock_is_held_and_never_reaps_it() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let busy = checkout(&root, "busy");
    // A lock whose recorded owner is certainly dead: an inspector must still
    // neither verify through it nor remove it.
    let lock = busy.join(".oak/wdlock");
    std::fs::write(&lock, "999999999").unwrap();

    let value = inventory(&root, &[]);
    let b = entry(&value, "/busy");
    assert_eq!(b["local_verification"], "unverified");
    assert!(b["local_verification_reason"]
        .as_str()
        .unwrap()
        .starts_with("checkout_locked: its workdir lock file exists"));
    assert_eq!(b["working_tree"], "unverified");
    assert_eq!(b["unpublished_commits"], "unverified");
    assert!(b.get("working_tree_change_count").is_none());
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), "999999999");
}

#[test]
fn verify_local_counts_only_commits_after_the_local_push_receipt() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let dir = checkout(&root, "linked");
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let pushed = repo.get_branch_head(&branch).unwrap().unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.example")
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    drop(repo);
    std::fs::write(
        dir.join(".oak/LAST_PUSHED_HEAD.json"),
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "remote": "https://oak.example",
            "owner": "acme",
            "repo": "repo",
            "branch": branch,
            "head": pushed.as_str(),
            "source": "local_push_receipt"
        }))
        .unwrap(),
    )
    .unwrap();

    let value = inventory(&root, &[]);
    let e = entry(&value, "/linked");
    assert_eq!(e["unpublished_commits"], "none");
    assert_eq!(e["unpublished_commit_count"], 0);
    assert_eq!(e["unpublished_commits_basis"], "local_push_receipt");

    std::fs::write(dir.join("a.txt"), "second\n").unwrap();
    ok(oak(&root, &dir, &["commit"]));
    std::fs::write(dir.join("a.txt"), "third\n").unwrap();
    ok(oak(&root, &dir, &["commit"]));
    let value = inventory(&root, &[]);
    let e = entry(&value, "/linked");
    assert_eq!(e["unpublished_commits"], "present");
    assert_eq!(e["unpublished_commit_count"], 2);
    assert_eq!(e["unpublished_commits_basis"], "local_push_receipt");

    // A receipt for another remote is not evidence for this one: fall back
    // to the labelled upper bound instead of claiming the branch is pushed.
    std::fs::write(
        dir.join(".oak/LAST_PUSHED_HEAD.json"),
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "remote": "https://elsewhere.example",
            "owner": "acme",
            "repo": "repo",
            "branch": branch,
            "head": pushed.as_str(),
            "source": "local_push_receipt"
        }))
        .unwrap(),
    )
    .unwrap();
    let value = inventory(&root, &[]);
    let e = entry(&value, "/linked");
    assert_eq!(
        e["unpublished_commits_basis"],
        "unmerged_commits_upper_bound"
    );
    assert_eq!(e["unpublished_commit_count"], 3);
}

#[tokio::test(flavor = "current_thread")]
async fn include_ci_reports_exact_head_state_with_one_listing_per_repository() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let one = checkout(&root, "one");
    let two = checkout(&root, "two");
    let server = MockServer::start().await;
    let mut heads = Vec::new();
    for dir in [&one, &two] {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        let branch = repo.get_current_branch_name().unwrap().unwrap();
        heads.push((
            branch.clone(),
            repo.get_branch_head(&branch).unwrap().unwrap(),
        ));
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
    }
    let run = |id: u64, branch: &str, commit: &str, status: &str, conclusion: Option<&str>| {
        json!({
            "id": id, "workflow_name": "ci", "event": "push", "branch": branch,
            "commit_hash": commit, "status": status, "conclusion": conclusion
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/acme/repo/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            run(
                12,
                &heads[0].0,
                heads[0].1.as_str(),
                "completed",
                Some("success")
            ),
            run(
                11,
                &heads[0].0,
                heads[0].1.as_str(),
                "completed",
                Some("failure")
            ),
            run(10, "other", &"f".repeat(64), "running", None),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let value = inventory(&root, &["--include-ci"]);
    let a = entry(&value, "/one");
    assert_eq!(a["ci"]["state"], "success");
    assert_eq!(a["ci"]["run_id"], 12);
    assert_eq!(a["ci"]["commit"], heads[0].1.as_str());
    assert_eq!(a["ci"]["run_branch_matches_checkout"], true);
    let b = entry(&value, "/two");
    assert_eq!(b["ci"]["state"], "not_found_in_recent_runs");
    assert!(b["ci"]["scope"].as_str().unwrap().contains("newest"));
}

struct ServeGuard(std::process::Child);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_serve(dir: &Path, token: &str) -> (ServeGuard, String) {
    oak_cli::http::ensure_crypto_provider();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--token",
            token,
        ])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let guard = ServeGuard(child);
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..200 {
        if reqwest::Client::new()
            .get(format!("{base}/api/capabilities"))
            .send()
            .await
            .is_ok()
        {
            return (guard, base);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("oak serve did not become ready");
}

/// QA W4b F4: a pushed current branch plus an unpushed commit on another
/// local branch must not read as `unpublished_commits: "none"`.
#[tokio::test(flavor = "current_thread")]
async fn verify_local_counts_unpublished_commits_on_other_local_branches() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let (_serve, base) = start_serve(&root.join("serve-data"), "inv-token").await;
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(&ws)
            .env("HOME", root.join(".home"))
            .env("OAK_API_KEY", "inv-token")
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env("OAK_MOUNTS_ROOT", root.join("absent-test-mount-registry"))
            .env_remove("OAK_REMOTE")
            .output()
            .unwrap();
        ok(out)
    };
    run(&["init", "."]);
    std::fs::write(ws.join("a.txt"), "a\n").unwrap();
    run(&["commit"]);
    run(&["push", "--repo", "oak/inv", "-r", &base, "--json"]);
    let pushed_branch = SqliteRepository::open(&ws.join(".oak/oak.db"))
        .unwrap()
        .get_current_branch_name()
        .unwrap()
        .unwrap();

    // Pushed and clean: nothing unpublished anywhere.
    let value = inventory(&root, &[]);
    let e = entry(&value, "/ws");
    assert_eq!(e["unpublished_commits"], "none", "{e}");
    assert_eq!(e["unpublished_commits_scope"], "all_local_branches");

    run(&["switch", "-c", "side"]);
    std::fs::write(ws.join("side.txt"), "side\n").unwrap();
    run(&["commit"]);
    run(&["switch", &pushed_branch]);

    let value = inventory(&root, &[]);
    let e = entry(&value, "/ws");
    assert_eq!(e["local_verification"], "verified");
    assert_eq!(e["branch"], pushed_branch.as_str());
    assert_eq!(e["working_tree"], "clean");
    assert_eq!(e["unpublished_commits"], "present", "{e}");
    assert_eq!(e["unpublished_commit_count"], 1);
    let rows = e["unpublished_by_branch"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{e}");
    assert_eq!(rows[0]["branch"], "side");
    assert_eq!(rows[0]["count"], 1);
    assert_eq!(rows[0]["basis"], "unmerged_commits_upper_bound");
}

#[test]
fn verify_local_reports_in_progress_merge_or_sync_as_unverified() {
    let temp = tempdir();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for (name, marker) in [("merging", "MERGE_HEAD"), ("syncing", "SYNC_STATE")] {
        let dir = checkout(&root, name);
        std::fs::write(dir.join(".oak").join(marker), "x").unwrap();
    }
    let value = inventory(&root, &[]);
    for (name, marker) in [("/merging", "MERGE_HEAD"), ("/syncing", "SYNC_STATE")] {
        let e = entry(&value, name);
        assert_eq!(e["local_verification"], "unverified");
        assert_eq!(
            e["local_verification_reason"],
            format!("operation_in_progress: {marker}")
        );
        assert_eq!(e["working_tree"], "unverified");
    }
}
