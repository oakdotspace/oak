use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn run(id: u64, commit: &str, event: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "workflow_name": "ci",
        "event": event,
        "branch": "main",
        "commit_hash": commit,
        "status": status,
        "conclusion": if status == "completed" { Some("success") } else { None },
        "error": null,
        "finished_at": if status == "completed" { Some("2026-09-07T00:00:00Z") } else { None::<&str> },
        "jobs": []
    })
}

#[tokio::test]
async fn conflict_refetch_does_not_confirm_a_different_event_as_the_same_run() {
    let commit = "a".repeat(64);
    let server = MockServer::builder().start().await;
    let get_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with({
            let get_count = Arc::clone(&get_count);
            let commit = commit.clone();
            move |_: &wiremock::Request| {
                if get_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200).set_body_json(run(42, &commit, "push", "running"))
                } else {
                    ResponseTemplate::new(200).set_body_json(run(
                        42,
                        &commit,
                        "manual",
                        "completed",
                    ))
                }
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/42/cancel"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;

    let api = oak_cli::commands::ci::CiClient {
        remote: server.uri(),
        owner: "oak".into(),
        repo: "oak".into(),
        token: None,
    };
    let error = api.cancel_exact(42, &commit).await.unwrap_err().to_string();
    assert!(error.contains("cancellation_not_confirmed"), "{error}");
    assert!(!error.contains("terminal no-op confirmed"), "{error}");
}
