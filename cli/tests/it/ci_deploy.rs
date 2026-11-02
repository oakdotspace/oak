//! `oak ci deploy` — release preview, confirmation, and the exact-value
//! release authorization, run as the real binary against wiremock.
//!
//! Exclusive `MockServer::builder().start()` listeners, not pooled
//! `MockServer::start()` addresses (see the note in `ci.rs`).

use std::path::Path;

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PREVIEW_PATH: &str = "/api/oak/oak/ci/environments/production/release-preview";
const AUTHORIZE_PATH: &str = "/api/oak/oak/ci/release-authorizations";
const API_KEY: &str = "deploy-test-key";

fn commit() -> String {
    "c".repeat(64)
}

fn fixture_repo(dir: &Path, remote: &str) {
    let oak_dir = dir.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.store_branch(&Branch::new(
        "tester-ci".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("tester-ci").unwrap();
    repo.put_commit_and_advance_refs(
        "tester-ci".to_string(),
        None,
        None,
        Vec::new(),
        "tester".to_string(),
        None,
        chrono::Utc::now(),
        Vec::new(),
    )
    .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
}

fn preview() -> Value {
    json!({
        "environment": "production",
        "branch": "main",
        "commit": commit(),
        "workflow": "release-macos",
        "workflow_path": ".oak/workflows/release-macos.yml",
        "workflow_hash": "d".repeat(64),
        "event": "manual",
        "policy_version": 7,
        "required_reviewers": true,
        "reviewers": ["zach"],
        "runner_online": true,
        "can_authorize": true,
        "ready": true,
        "problems": [],
    })
}

/// The authorization body: exactly the preview's values, nothing else.
fn expected_request() -> Value {
    json!({
        "environment": "production",
        "branch": "main",
        "commit": commit(),
        "workflow_path": ".oak/workflows/release-macos.yml",
        "workflow_hash": "d".repeat(64),
        "event": "manual",
        "policy_version": 7,
    })
}

async fn mount_preview(server: &MockServer, body: Value, calls: u64) {
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .and(header(
            "authorization",
            format!("Bearer {API_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(calls)
        .mount(server)
        .await;
}

/// A POST mock that must never be hit.
async fn forbid_authorize(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

async fn mount_authorize(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(AUTHORIZE_PATH))
        .and(header(
            "authorization",
            format!("Bearer {API_KEY}").as_str(),
        ))
        .and(body_json(expected_request()))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

fn receipt() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"authorization_id": "e".repeat(64), "run_id": 1234}))
}

async fn deploy(dir: &Path, args: &[&str]) -> std::process::Output {
    let dir = dir.to_owned();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .current_dir(dir)
            .args(["ci", "deploy"])
            .args(args)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_API_KEY", API_KEY)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn stdout_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[tokio::test]
async fn deploy_yes_authorizes_exactly_the_previewed_values() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .and(query_param("workflow", "release-macos"))
        .and(query_param("branch", "main"))
        .and(header(
            "authorization",
            format!("Bearer {API_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(preview()))
        .expect(1)
        .mount(&server)
        .await;
    mount_authorize(&server, receipt()).await;

    let output = deploy(
        temp.path(),
        &[
            "-e",
            "production",
            "--workflow",
            "release-macos",
            "--branch",
            "main",
            "--yes",
        ],
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("main@cccccccccccc"), "{stdout}");
    assert!(stdout.contains("version 7"), "{stdout}");
    assert!(stdout.contains("zach"), "{stdout}");
    assert!(stdout.contains("#1234"), "{stdout}");
    assert!(
        stdout.contains(&format!("{}/oak/oak/ci/runs/1234", server.uri())),
        "{stdout}"
    );
}

#[tokio::test]
async fn deploy_json_reports_preview_authorization_and_wait_follow_up() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    let mut body = preview();
    body["future_field"] = json!("kept");
    mount_preview(&server, body, 1).await;
    mount_authorize(&server, receipt()).await;

    let output = deploy(temp.path(), &["-e", "production", "--yes", "--json"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc = stdout_json(&output);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["outcome"], "authorized");
    assert_eq!(doc["preview"]["commit"], commit());
    assert_eq!(doc["preview"]["future_field"], "kept");
    assert_eq!(doc["authorization"]["run_id"], 1234);
    assert_eq!(doc["authorization"]["authorization_id"], "e".repeat(64));
    assert_eq!(
        doc["authorization"]["run_url"],
        format!("{}/oak/oak/ci/runs/1234", server.uri())
    );
    assert_eq!(doc["authorization"]["request"], expected_request());
    assert!(doc.get("wait").is_none());
    assert_eq!(
        doc["recommended_next_commands"],
        json!([format!(
            "oak ci wait 1234 --commit {} --json --summary",
            commit()
        )])
    );
}

#[tokio::test]
async fn deploy_not_ready_exits_nonzero_without_authorizing() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    let mut body = preview();
    body["ready"] = json!(false);
    body["runner_online"] = json!(false);
    body["problems"] = json!(["none of the environment's runners is online"]);
    mount_preview(&server, body, 2).await;
    forbid_authorize(&server).await;

    let output = deploy(temp.path(), &["-e", "production", "--yes"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("none of the environment's runners is online"),
        "{stdout}"
    );
    assert!(stderr.contains("not ready"), "{stderr}");

    let output = deploy(temp.path(), &["-e", "production", "--yes", "--json"]).await;
    assert_eq!(output.status.code(), Some(1));
    let doc = stdout_json(&output);
    assert_eq!(doc["outcome"], "not_ready");
    assert_eq!(
        doc["preview"]["problems"],
        json!(["none of the environment's runners is online"])
    );
    assert!(doc.get("authorization").is_none());
}

#[tokio::test]
async fn deploy_caller_who_cannot_authorize_exits_nonzero_without_authorizing() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    let mut body = preview();
    body["can_authorize"] = json!(false);
    body["reviewers"] = json!(["zach", "alice"]);
    mount_preview(&server, body, 2).await;
    forbid_authorize(&server).await;

    let output = deploy(temp.path(), &["-e", "production", "--yes"]).await;
    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot approve"), "{stderr}");
    assert!(stderr.contains("zach, alice"), "{stderr}");

    let output = deploy(temp.path(), &["-e", "production", "--yes", "--json"]).await;
    assert_eq!(output.status.code(), Some(6));
    let doc = stdout_json(&output);
    assert_eq!(doc["outcome"], "cannot_authorize");
    assert_eq!(
        doc["recommended_next_commands"],
        json!(["oak auth status --json"])
    );
}

#[tokio::test]
async fn deploy_dry_run_never_authorizes() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(preview()))
        .expect(2)
        .mount(&server)
        .await;
    forbid_authorize(&server).await;

    // --yes does not turn a dry run into a deploy.
    let output = deploy(temp.path(), &["-e", "production", "--dry-run", "--yes"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nothing was authorized"), "{stdout}");

    let output = deploy(temp.path(), &["-e", "production", "--dry-run", "--json"]).await;
    assert_eq!(output.status.code(), Some(0));
    let doc = stdout_json(&output);
    assert_eq!(doc["outcome"], "dry_run");
    assert!(doc.get("authorization").is_none());
    assert_eq!(
        doc["recommended_next_commands"],
        json!(["oak ci deploy -e production --workflow .oak/workflows/release-macos.yml --branch main --yes --json"])
    );
}

#[tokio::test]
async fn deploy_without_yes_and_without_a_tty_refuses() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(preview()))
        .expect(2)
        .mount(&server)
        .await;
    forbid_authorize(&server).await;

    let output = deploy(temp.path(), &["-e", "production"]).await;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");

    // --json never prompts, even on a terminal.
    let output = deploy(temp.path(), &["-e", "production", "--json"]).await;
    assert_eq!(output.status.code(), Some(2));
    let doc = stdout_json(&output);
    assert_eq!(doc["outcome"], "confirmation_required");
}

#[tokio::test]
async fn deploy_authorization_conflict_surfaces_the_server_message() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(preview()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(AUTHORIZE_PATH))
        .and(body_json(expected_request()))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(
                json!({"error": "Branch 'main' moved since this release was reviewed"}),
            ),
        )
        .expect(2)
        .mount(&server)
        .await;

    let output = deploy(temp.path(), &["-e", "production", "--yes"]).await;
    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Branch 'main' moved since this release was reviewed"),
        "{stderr}"
    );
    assert!(stderr.contains("Nothing was authorized"), "{stderr}");

    let output = deploy(temp.path(), &["-e", "production", "--yes", "--json"]).await;
    assert_eq!(output.status.code(), Some(6));
    let doc = stdout_json(&output);
    let message = doc["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Branch 'main' moved since this release was reviewed"),
        "{message}"
    );
}

#[tokio::test]
async fn deploy_preview_errors_surface_the_server_message() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path(PREVIEW_PATH))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "Several workflows deploy to 'production' (a, b); name one"
        })))
        .expect(1)
        .mount(&server)
        .await;
    forbid_authorize(&server).await;

    let output = deploy(temp.path(), &["-e", "production", "--yes"]).await;
    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Several workflows deploy"), "{stderr}");
    assert!(stderr.contains("--workflow"), "{stderr}");
}

#[tokio::test]
async fn deploy_refuses_a_preview_for_another_branch() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_preview(&server, preview(), 1).await;
    forbid_authorize(&server).await;

    let output = deploy(
        temp.path(),
        &["-e", "production", "--branch", "release", "--yes"],
    )
    .await;
    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("branch 'main'"), "{stderr}");
}

#[tokio::test]
async fn deploy_wait_embeds_the_exact_run_wait_and_exits_with_its_status() {
    for (conclusion, code) in [("success", 0), ("failure", 1)] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        fixture_repo(temp.path(), &server.uri());
        mount_preview(&server, preview(), 1).await;
        mount_authorize(&server, receipt()).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1234,
                "workflow_name": "release-macos",
                "workflow_path": ".oak/workflows/release-macos.yml",
                "event": "manual",
                "branch": "main",
                "commit_hash": commit(),
                "status": "completed",
                "conclusion": conclusion,
                "started_at": "2026-10-13T10:00:00+00:00",
                "finished_at": "2026-10-13T10:04:00+00:00",
                "jobs": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let output = deploy(
            temp.path(),
            &["-e", "production", "--yes", "--wait=5", "--json"],
        )
        .await;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let doc = stdout_json(&output);
        assert_eq!(doc["outcome"], "authorized");
        assert_eq!(doc["authorization"]["run_id"], 1234);
        assert_eq!(doc["wait"]["projection"], "summary");
        assert_eq!(doc["wait"]["expected_commit"], commit());
        assert_eq!(doc["wait"]["requested_run_ids"], json!([1234]));
        assert_eq!(doc["wait"]["state"], conclusion);
        assert_eq!(
            doc["recommended_next_commands"],
            doc["wait"]["recommended_next_commands"]
        );
    }
}

#[tokio::test]
async fn deploy_wait_rejects_a_run_for_another_commit() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_preview(&server, preview(), 1).await;
    mount_authorize(&server, receipt()).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1234,
            "workflow_name": "release-macos",
            "event": "manual",
            "branch": "main",
            "commit_hash": "f".repeat(64),
            "status": "completed",
            "conclusion": "success",
        })))
        .mount(&server)
        .await;

    let output = deploy(
        temp.path(),
        &["-e", "production", "--yes", "--wait", "--json"],
    )
    .await;
    assert_eq!(output.status.code(), Some(6));
    let doc = stdout_json(&output);
    // The authorization still reaches the caller, with the wait's failure.
    assert_eq!(doc["outcome"], "authorized");
    assert_eq!(doc["authorization"]["run_id"], 1234);
    assert!(doc.get("wait").is_none());
    assert!(
        doc["wait_error"].as_str().unwrap().contains("refusing"),
        "{doc}"
    );
}
