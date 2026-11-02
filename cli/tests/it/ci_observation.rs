use std::process::{Command, Stdio};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_summary_is_compact_and_failed_projection_is_utf8_bounded() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    repo.store_branch(&Branch::new("review".into(), None, None))
        .unwrap();
    repo.set_current_branch("review").unwrap();
    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    let run = json!({"id":42,"commit_hash":"a".repeat(64),"branch":"review",
    "status":"completed","conclusion":"failure","jobs":[{"name":"check","steps":[
        {"id":1,"name":"build","status":"completed","conclusion":"success","logs":"noise".repeat(10000)},
        {"id":2,"name":"test","status":"completed","conclusion":"failure","exit_code":101,"logs":"échec échec","command":"private script"}
    ]}]});
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run))
        .mount(&server)
        .await;
    let invoke = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(temp.path())
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_API_KEY")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let summary = invoke(&["ci", "logs", "42", "--summary", "--json"]);
    assert_eq!(summary["run"]["id"], 42);
    assert_eq!(
        summary["run"]["jobs"][0]["steps"].as_array().unwrap().len(),
        2
    );
    assert!(summary["run"]["jobs"][0]["steps"][1].get("logs").is_none());
    assert!(summary.to_string().len() < 2000);
    let failed = invoke(&["ci", "logs", "42", "--failed", "--max-bytes", "5", "--json"]);
    let steps = failed["run"]["jobs"][0]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["id"], 2);
    assert!(steps[0]["logs"].as_str().unwrap().len() <= 5);
    assert_eq!(failed["logs_truncated"], true);
    assert_eq!(
        failed["run_url"],
        format!("{}/oak/oak/ci/runs/42", server.uri())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_run_status_does_not_use_checkout_head_and_preserves_wait_fence() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    let head = "a".repeat(64);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":42,"commit_hash":head,"branch":"other-branch","status":"running"
        })))
        .mount(&server)
        .await;
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["ci", "status", "--run", "42", "--commit", &head, "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["branch"], "other-branch");
    assert_eq!(value["commit"], head);
    assert_eq!(
        value["run_url"],
        format!("{}/oak/oak/ci/runs/42", server.uri())
    );
    assert_eq!(
        value["recommended_next_commands"][0],
        format!("oak ci wait 42 --commit {head} --json")
    );
    let mismatch = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "ci",
            "status",
            "--run",
            "42",
            "--commit",
            &"b".repeat(64),
            "--json",
        ])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    let error: Value = serde_json::from_slice(&mismatch.stdout).unwrap();
    assert!(error.get("error").is_some(), "{error}");
    server.verify().await;
}
