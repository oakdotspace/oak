//! Branch assessment robustness (fb-118, fb-120, fb-327).
//!
//! - fb-118: one stored branch row with an undecodable `created_at` must not
//!   abort `oak branch triage` or unrelated `oak branch review`; the bad row
//!   becomes a typed per-branch error and every other branch is assessed.
//! - fb-120/fb-327: `oak branch review --json` and `oak branch triage --json`
//!   fill `checks` from exact-head CI evidence (one runs listing), and
//!   `merge_allowed` is true only when the existing gates also hold.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
/// contributing branch `feature-clean` (recommended validate_then_merge,
/// vcs_merge_safe true) and a second branch `other`.
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    repo.set_metadata(MetadataKey::MainLastCheckedAt, &now.to_string())
        .unwrap();
    let head = repo
        .get_branch_head("feature-clean")
        .unwrap()
        .unwrap()
        .to_string();
    (temp, head)
}

fn link_remote(dir: &Path, remote: &str) {
    let repo = db(dir);
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
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

fn run(id: u64, branch: &str, commit: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({
        "id": id,
        "workflow_name": "ci",
        "workflow_path": ".oak/workflows/ci.yml",
        "event": "push",
        "branch": branch,
        "commit_hash": commit,
        "status": status,
        "conclusion": conclusion,
    })
}

async fn ci_server(runs: Vec<Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/widgets/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(runs)))
        .mount(&server)
        .await;
    server
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
    assert_eq!(good["recommended_action"], "validate_then_merge");

    // Unrelated single-branch review is unaffected by the corrupt row.
    let single = review(dir, "feature-clean");
    assert_eq!(single["recommended_action"], "validate_then_merge");
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

// --- fb-120 / fb-327 -----------------------------------------------------

#[test]
fn no_remote_reports_not_queried_with_reason() {
    let (temp, _head) = fixture();
    let checks = &review(temp.path(), "feature-clean")["checks"];
    assert_eq!(checks["state"], "not_queried");
    assert_eq!(checks["reason"], "no_remote_configured");
    assert_eq!(checks["known_passed"], false);
    assert!(checks["source"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_head_success_passes_checks_and_allows_merge() {
    let (temp, head) = fixture();
    let dir = temp.path();
    let server = ci_server(vec![
        run(12, "feature-clean", &head, "completed", Some("success")),
        run(
            11,
            "feature-clean",
            &"a".repeat(64),
            "completed",
            Some("failure"),
        ),
    ])
    .await;
    link_remote(dir, &server.uri());

    let dir_owned = dir.to_path_buf();
    let json = tokio::task::spawn_blocking(move || review(&dir_owned, "feature-clean"))
        .await
        .unwrap();
    let checks = &json["checks"];
    assert_eq!(checks["state"], "success", "{json}");
    assert_eq!(checks["known_passed"], true);
    assert_eq!(checks["run_id"], 12);
    assert_eq!(checks["head"], head.as_str());
    assert_eq!(checks["source"], format!("ci_run:12@{head}"));
    assert!(checks["run_url"]
        .as_str()
        .unwrap()
        .ends_with("/acme/widgets/ci/runs/12"));
    assert_eq!(json["vcs_merge_safe"], true);
    assert_eq!(json["merge_allowed"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn run_for_a_different_head_is_never_borrowed() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    let server = ci_server(vec![run(
        20,
        "feature-clean",
        &"b".repeat(64),
        "completed",
        Some("success"),
    )])
    .await;
    link_remote(dir, &server.uri());

    let dir_owned = dir.to_path_buf();
    let json = tokio::task::spawn_blocking(move || review(&dir_owned, "feature-clean"))
        .await
        .unwrap();
    let checks = &json["checks"];
    assert_eq!(checks["state"], "no_runs", "{json}");
    assert_eq!(checks["known_passed"], false);
    assert!(checks.get("run_id").is_none());
    assert_eq!(json["merge_allowed"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_and_running_heads_do_not_pass() {
    let (temp, head) = fixture();
    let dir = temp.path();
    let other_head = db(dir)
        .get_branch_head("other")
        .unwrap()
        .unwrap()
        .to_string();
    let server = ci_server(vec![
        run(31, "feature-clean", &head, "completed", Some("failure")),
        run(32, "other", &other_head, "running", None),
    ])
    .await;
    link_remote(dir, &server.uri());

    let dir_owned = dir.to_path_buf();
    let triage = tokio::task::spawn_blocking(move || {
        json_stdout(&oak(&dir_owned, &["branch", "triage", "--json"]))
    })
    .await
    .unwrap();
    // One listing request serves the whole batch.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let failed = triage_row(&triage, "feature-clean");
    assert_eq!(failed["checks"]["state"], "failure", "{triage}");
    assert_eq!(failed["checks"]["known_passed"], false);
    assert_eq!(failed["checks"]["run_id"], 31);
    assert_eq!(failed["merge_allowed"], false);

    let running = triage_row(&triage, "other");
    assert_eq!(running["checks"]["state"], "running");
    assert_eq!(running["checks"]["known_passed"], false);
    assert_eq!(running["checks"]["run_id"], 32);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_ci_is_reported_unavailable() {
    let (temp, _head) = fixture();
    let dir = temp.path();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/widgets/ci/runs"))
        .respond_with(ResponseTemplate::new(500).set_body_string("database exploded: secret"))
        .mount(&server)
        .await;
    link_remote(dir, &server.uri());

    let dir_owned = dir.to_path_buf();
    let (json, triage) = tokio::task::spawn_blocking(move || {
        (
            review(&dir_owned, "feature-clean"),
            json_stdout(&oak(&dir_owned, &["branch", "triage", "--json"])),
        )
    })
    .await
    .unwrap();
    for checks in [
        &json["checks"],
        &triage_row(&triage, "feature-clean")["checks"],
    ] {
        assert_eq!(checks["state"], "unavailable", "{checks}");
        assert_eq!(checks["source"], "unavailable");
        assert_eq!(checks["known_passed"], false);
        assert!(!checks.to_string().contains("secret"));
    }
    // Existing gates hold but CI evidence is missing: not allowed.
    assert_eq!(json["vcs_merge_safe"], true);
    assert_eq!(json["merge_allowed"], false);
}

/// QA-L5 probe: the server merge gate takes the newest run per workflow for
/// the commit across all branches; a newer same-commit failure on another
/// branch blocks it. The client must not advise merge_allowed:true.
#[tokio::test(flavor = "multi_thread")]
async fn qa_l5_other_branch_newer_failure_blocks_merge_allowed() {
    let (temp, head) = fixture();
    let dir = temp.path();
    let server = ci_server(vec![
        run(13, "qa-copy", &head, "completed", Some("failure")),
        run(12, "feature-clean", &head, "completed", Some("success")),
    ])
    .await;
    link_remote(dir, &server.uri());
    let dir_owned = dir.to_path_buf();
    let json = tokio::task::spawn_blocking(move || review(&dir_owned, "feature-clean"))
        .await
        .unwrap();
    assert_eq!(
        json["merge_allowed"], false,
        "server gate would refuse (newest ci run for head is #13 failure) but client says {}",
        json["checks"]
    );
}

/// QA-L5 L4: a dirty current-branch worktree never reports merge_allowed,
/// even with passing exact-head CI and a certified-safe prediction.
#[tokio::test(flavor = "multi_thread")]
async fn dirty_current_worktree_withholds_merge_allowed() {
    let (temp, head) = fixture();
    let dir = temp.path();
    let server = ci_server(vec![run(
        12,
        "feature-clean",
        &head,
        "completed",
        Some("success"),
    )])
    .await;
    link_remote(dir, &server.uri());
    std::fs::write(dir.join("uncommitted.txt"), "dirty\n").unwrap();
    let dir_owned = dir.to_path_buf();
    let json = tokio::task::spawn_blocking(move || review(&dir_owned, "feature-clean"))
        .await
        .unwrap();
    assert_eq!(json["checks"]["known_passed"], true, "{json}");
    assert_eq!(json["merge_allowed"], false);
    assert!(json["caveats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c.as_str().unwrap().contains("uncommitted changes")));
}

/// QA-L5 R2-M1: a full listing page cannot prove a success (an older
/// failing run of the head may be outside it); report unavailable with a
/// caveat, while merge_allowed stays false.
#[tokio::test(flavor = "multi_thread")]
async fn full_runs_page_reports_scan_window_incomplete() {
    let (temp, head) = fixture();
    let dir = temp.path();
    let mut runs = vec![run(
        500,
        "feature-clean",
        &head,
        "completed",
        Some("success"),
    )];
    for id in (400..499).rev() {
        runs.push(run(
            id,
            "noise",
            &"c".repeat(64),
            "completed",
            Some("success"),
        ));
    }
    let server = ci_server(runs).await;
    link_remote(dir, &server.uri());
    let dir_owned = dir.to_path_buf();
    let json = tokio::task::spawn_blocking(move || review(&dir_owned, "feature-clean"))
        .await
        .unwrap();
    assert_eq!(json["checks"]["state"], "unavailable", "{json}");
    assert_eq!(json["checks"]["reason"], "scan_window_incomplete");
    assert_eq!(json["checks"]["known_passed"], false);
    assert_eq!(json["merge_allowed"], false);
    assert!(json["caveats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c.as_str().unwrap().contains("per-commit")));
}
