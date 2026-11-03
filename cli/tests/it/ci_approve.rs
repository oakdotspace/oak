//! `oak ci approve` client: exact-run preflight, then the approve POST.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn run(id: u64, commit: &str, status: &str, awaiting_approval: bool) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "workflow_name": "ci",
        "event": "push",
        "branch": "feature",
        "commit_hash": commit,
        "triggered_by": "contractor",
        "status": status,
        "conclusion": if status == "completed" { Some("success") } else { None },
        "awaiting_approval": awaiting_approval,
        "jobs": []
    })
}

async fn server_with(run_body: serde_json::Value, post_status: u16, posts: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_body))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/42/approve"))
        .respond_with(ResponseTemplate::new(post_status))
        .expect(posts)
        .mount(&server)
        .await;
    server
}

fn client(server: &MockServer) -> oak_cli::commands::ci::CiClient {
    oak_cli::commands::ci::CiClient {
        remote: server.uri(),
        owner: "oak".into(),
        repo: "oak".into(),
        token: None,
    }
}

#[tokio::test]
async fn approves_an_exact_held_run() {
    let commit = "a".repeat(64);
    let server = server_with(run(42, &commit, "queued", true), 204, 1).await;
    let approved = client(&server).approve_exact(42, &commit).await.unwrap();
    assert_eq!(approved.id, 42);
    assert_eq!(approved.triggered_by.as_deref(), Some("contractor"));
}

#[tokio::test]
async fn refuses_without_posting_when_the_run_is_not_held() {
    let commit = "a".repeat(64);
    for body in [
        run(42, &commit, "queued", false),
        run(42, &commit, "completed", true),
    ] {
        let server = server_with(body, 204, 0).await;
        let error = client(&server)
            .approve_exact(42, &commit)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not awaiting approval"), "{error}");
    }
}

#[tokio::test]
async fn refuses_without_posting_when_the_commit_does_not_match() {
    let server = server_with(run(42, &"a".repeat(64), "queued", true), 204, 0).await;
    let error = client(&server)
        .approve_exact(42, &"b".repeat(64))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match"), "{error}");

    // A short hash is refused before any request at all.
    let error = client(&server)
        .approve_exact(42, "aaaa")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("64-character"), "{error}");
}

#[tokio::test]
async fn explains_a_forbidden_approval() {
    let commit = "a".repeat(64);
    let server = server_with(run(42, &commit, "queued", true), 403, 1).await;
    let error = client(&server)
        .approve_exact(42, &commit)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("read every path"), "{error}");
}
