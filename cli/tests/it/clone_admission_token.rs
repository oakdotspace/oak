//! fb-621: the clone admission (integrity snapshot) token.
//!
//! A server that encoded every branch head into the token stopped serving
//! full clones once a repository had a few hundred branches: the token no
//! longer fit the pull request URL. These tests drive the CLI against a mock
//! server that still issues an oversized token. The end-to-end clones of a
//! repository with 1,500 branches run against the real server.

use std::path::Path;
use std::process::Output;

use oak_cli::commands::repo;
use oak_core::{Commit, Tree};
use serde_json::Value;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

// ---------------------------------------------------------------------------
// A server that still issues a list-form (oversized) token
// ---------------------------------------------------------------------------

/// Longer than any URL the HTTP client can send (65,534 bytes).
const OVERSIZED_TOKEN_BYTES: usize = 70_000;

fn oversized_token() -> String {
    "QUJD".repeat(OVERSIZED_TOKEN_BYTES / 4)
}

fn healthy_report(token: &str, depth: Option<u32>) -> Value {
    serde_json::json!({
        "schema_version": 1,
        "repo": "oak/repo",
        "status": "healthy",
        "healthy": true,
        "complete": true,
        "truncated": false,
        "verification": "metadata",
        "proof_profile": "bounded_v1",
        "snapshot_token": token,
        "scope": {"depth": depth, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
        "findings": []
    })
}

async fn mount_capabilities(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .mount(server)
        .await;
}

/// Full-history preflights get the oversized token; depth-one preflights get
/// a small one and a servable one-commit pull, like a real list-form server.
async fn mount_list_form_server(server: &MockServer) {
    mount_capabilities(server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param_is_missing("depth"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(healthy_report(&oversized_token(), None)),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("depth", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(healthy_report("shallow-token", Some(1))),
        )
        .mount(server)
        .await;
    let tree = Tree::new(Vec::new()).unwrap();
    let main = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let branch = serde_json::json!({
        "name": "main",
        "description": null,
        "parent_branch": null,
        "status": "open",
        "created_at": "2026-01-01T00:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("depth", "1"))
        .and(query_param("integrity_snapshot", "shallow-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": main.hash,
            "branch": branch,
            "branches": [branch],
            "commits": [{
                "hash": main.hash,
                "branch_name": main.branch_name,
                "parent_hash": main.parent_hash,
                "merge_parent_hash": main.merge_parent_hash,
                "manifest_hash": main.manifest_hash,
                "author": main.author,
                "message": main.message,
                "timestamp": main.timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .mount(server)
        .await;
}

async fn pull_requests(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/api/oak/repo/pull")
        .collect()
}

fn oak_async(home: &Path, cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1");
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_admission_token_is_a_compact_typed_error_with_a_working_retry() {
    let server = MockServer::start().await;
    mount_list_form_server(&server).await;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let uri = server.uri();

    let output = oak_async(&home, &work)
        .args(["clone", "oak/repo", "my dest", "--remote", &uri, "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(6),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.len() < 2048,
        "stdout is {} bytes",
        output.stdout.len()
    );
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(!stdout.contains("QUJDQUJD"), "the token is never echoed");
    assert!(output.stderr.len() < 2048);
    let envelope = json(&output);
    let error = &envelope["error"];
    assert_eq!(error["code"], "clone_admission_token_too_large");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("is {OVERSIZED_TOKEN_BYTES} bytes")),
        "{message}"
    );
    assert!(message.contains("'oak/repo'"), "{message}");
    assert!(
        message.contains("NOT equivalent to the full clone")
            && message.contains("no earlier history"),
        "{message}"
    );
    assert!(!message.contains("fb-"), "{message}");
    let retry = format!("oak clone --shallow oak/repo 'my dest' --remote {uri} --json");
    assert_eq!(
        error["recommended_next_commands"],
        serde_json::json!([retry])
    );
    // Refused before the pull request: nothing was sent, nothing created,
    // and the full clone was not silently downgraded to a shallow one.
    assert!(pull_requests(&server).await.is_empty());
    assert!(!work.join("my dest").exists());

    // The recommended command works verbatim from the caller's directory.
    let oak_dir = Path::new(env!("CARGO_BIN_EXE_oak")).parent().unwrap();
    let path_var = std::env::join_paths(
        std::iter::once(oak_dir.to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let retried = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&retry)
        .current_dir(&work)
        .env("PATH", path_var)
        .env("HOME", &home)
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        retried.status.success(),
        "retry failed: stdout={} stderr={}",
        String::from_utf8_lossy(&retried.stdout),
        String::from_utf8_lossy(&retried.stderr)
    );
    let receipt = json(&retried);
    assert_eq!(receipt["request"]["history"]["mode"], "shallow");
    assert!(work.join("my dest/.oak/oak.db").exists());
    assert_eq!(pull_requests(&server).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_admission_token_text_error_is_bounded_and_keeps_every_flag() {
    let server = MockServer::start().await;
    mount_list_form_server(&server).await;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let uri = server.uri();

    // Human output, a deprecated `--full`, and a sparse cone.
    let output = oak_async(&root, &root)
        .args([
            "clone", "--full", "-r", &uri, "oak/repo", "--path", "docs", "--", "dest",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.len() < 512, "only the progress line");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.len() < 2048, "stderr is {} bytes", stderr.len());
    assert!(!stderr.contains("QUJDQUJD"));
    assert!(
        stderr.contains(&format!(
            "If a clone without history is acceptable: oak clone --shallow -r {uri} oak/repo --path docs -- dest"
        )),
        "{stderr}"
    );
    assert!(pull_requests(&server).await.is_empty());
    assert!(!root.join("dest").exists());
}

/// The refusal is about what can be sent, not a new size policy: a large
/// token that still fits the URL is passed through unchanged.
#[tokio::test]
async fn large_admission_token_that_fits_the_url_is_still_sent() {
    let server = MockServer::start().await;
    mount_capabilities(&server).await;
    let token = "QUJD".repeat(15_000);
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(healthy_report(&token, None)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", token.as_str()))
        .respond_with(ResponseTemplate::new(503).set_body_string("stop after the request"))
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let error = repo::clone_repo(&server.uri(), "oak/repo", &destination, false)
        .await
        .expect_err("the mock pull stops the clone");
    assert!(
        !matches!(
            error,
            oak_core::OakError::CloneAdmissionTokenTooLarge { .. }
        ),
        "{error}"
    );
    assert_eq!(pull_requests(&server).await.len(), 1);
    assert!(!destination.exists());
}

/// A library caller (not an `oak clone` process) gets the same typed,
/// bounded error. Its retry command is derived from the process's own
/// arguments, which here are the test runner's, so only its size is checked.
#[tokio::test]
async fn oversized_admission_token_is_typed_for_library_callers() {
    let server = MockServer::start().await;
    mount_list_form_server(&server).await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let error = repo::clone_repo(&server.uri(), "oak/repo", &destination, false)
        .await
        .expect_err("an unsendable token stops the clone");
    match &error {
        oak_core::OakError::CloneAdmissionTokenTooLarge {
            detail,
            recommended_next_commands,
        } => {
            assert!(detail.len() < 1024, "{detail}");
            assert!(!detail.contains("QUJDQUJD"));
            assert!(recommended_next_commands.len() <= 1);
            assert!(recommended_next_commands
                .iter()
                .all(|command| command.len() < 1024 && !command.contains("QUJDQUJD")));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(pull_requests(&server).await.is_empty());
    assert!(!destination.exists());
}
