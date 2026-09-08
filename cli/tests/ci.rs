//! `oak ci` — the CLI surface over the server's CI runs API.
//!
//! Exercises the real reqwest client against wiremock-served responses:
//! listing runs, the merge-gate status for the current branch head (and its
//! script-facing exit codes), step logs (including the fb-30 shape where the
//! server embeds raw control characters in `logs` — invalid strict JSON that
//! the CLI must parse defensively rather than crash on), and manual re-runs.
//!
//! Use exclusive `MockServer::builder().start()` listeners, not pooled
//! `MockServer::start()` addresses: the process-wide HTTP client can otherwise
//! reuse a connection owned by a previous test's stopped Tokio runtime and
//! fail with `DispatchGone` before reaching the next fixture.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn ci_command(dir: &Path, args: &[&str]) -> std::process::Output {
    let dir = dir.to_owned();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .current_dir(dir)
            .arg("ci")
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ci_trigger_uses_exact_capability_and_returns_all_receipt_ids() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, head) = fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/capabilities"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ordinary_trigger_v1":1})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/api/oak/oak/ci/trigger"))
        .and(body_partial_json(serde_json::json!({"protocol":"ordinary_trigger_v1","branch":"tester-ci","expected_commit":head,"idempotency_key":"review-1","workflow":"ci"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"protocol":"ordinary_trigger_v1","branch":"tester-ci","commit_hash":head,"run_ids":[910,911],"replayed":false})))
        .expect(1).mount(&server).await;
    let output = ci_command(
        temp.path(),
        &[
            "trigger",
            "--expected-commit",
            &head,
            "--idempotency-key",
            "review-1",
            "--workflow",
            "ci",
            "--json",
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["run_ids"], serde_json::json!([910, 911]));
    assert_eq!(receipt["commit_hash"], head);
    assert_eq!(receipt["branch"], "tester-ci");
    assert_eq!(receipt["replayed"], false);
}

#[tokio::test]
async fn ci_trigger_unsupported_capability_never_posts() {
    for response in [
        ResponseTemplate::new(404),
        ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"ordinary_trigger_v1":2})),
        ResponseTemplate::new(200).set_body_string("invalid"),
        ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
    ] {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/capabilities"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let api = oak_cli::commands::ci::CiClient {
            remote: server.uri(),
            owner: "oak".into(),
            repo: "oak".into(),
            token: None,
        };
        assert!(api
            .trigger_exact("main", &"a".repeat(64), "stable", None)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn ci_trigger_replay_and_rejections_never_retry_or_downgrade() {
    let head = "a".repeat(64);
    for status in [200, 409, 412, 503] {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/capabilities"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ordinary_trigger_v1":1})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/api/oak/oak/ci/trigger")).and(body_partial_json(serde_json::json!({"expected_commit":head,"idempotency_key":"stable"}))).respond_with(ResponseTemplate::new(status).set_body_json(serde_json::json!({"protocol":"ordinary_trigger_v1","branch":"main","commit_hash":head,"run_ids":[12],"replayed":true}))).expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let api = oak_cli::commands::ci::CiClient {
            remote: server.uri(),
            owner: "oak".into(),
            repo: "oak".into(),
            token: None,
        };
        let result = api.trigger_exact("main", &head, "stable", None).await;
        if status == 200 {
            assert!(result.unwrap().replayed);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(&status.to_string()), "{error}");
            assert!(error.contains("same idempotency key"));
        }
    }
}

#[tokio::test]
async fn ci_trigger_malformed_receipt_is_uncertain_not_success() {
    let head = "a".repeat(64);
    let valid = serde_json::json!({"protocol":"ordinary_trigger_v1","branch":"main","commit_hash":head,"run_ids":[12],"replayed":false});
    for (field, bad) in [
        ("protocol", serde_json::json!("v2")),
        ("branch", serde_json::json!("other")),
        ("commit_hash", serde_json::json!("b".repeat(64))),
        ("run_ids", serde_json::json!([])),
        ("run_ids", serde_json::json!([0])),
        ("run_ids", serde_json::json!([12, 12])),
        ("replayed", serde_json::Value::Null),
    ] {
        let server = MockServer::builder().start().await;
        let mut receipt = valid.clone();
        receipt[field] = bad;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ordinary_trigger_v1":1})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/trigger"))
            .respond_with(ResponseTemplate::new(200).set_body_json(receipt))
            .expect(1)
            .mount(&server)
            .await;
        let api = oak_cli::commands::ci::CiClient {
            remote: server.uri(),
            owner: "oak".into(),
            repo: "oak".into(),
            token: None,
        };
        let error = api
            .trigger_exact("main", &head, "stable", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("same idempotency key"), "{error}");
    }
}

fn cancel_api(remote: String) -> oak_cli::commands::ci::CiClient {
    oak_cli::commands::ci::CiClient {
        remote,
        owner: "oak".into(),
        repo: "oak".into(),
        token: Some("test-token".into()),
    }
}

fn cancellable_run(id: u64, commit: &str, event: &str) -> serde_json::Value {
    let mut run = run_json(id, "main", commit, "running", None);
    run["event"] = serde_json::json!(event);
    run
}

#[tokio::test]
async fn qa_cancel_malformed_metadata_does_not_echo_secret_values() {
    let mut leaks = Vec::new();
    for field in ["id", "jobs", "triggered_by"] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        let (_, commit) = fixture_repo(temp.path(), &server.uri());
        let mut run = cancellable_run(42, &commit, "push");
        run[field] = if field == "triggered_by" {
            serde_json::json!({"secret": "malformed-private-secret"})
        } else {
            serde_json::json!("malformed-private-secret")
        };
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(run))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        let output = ci_command(
            temp.path(),
            &["cancel", "42", "--commit", &commit, "--json"],
        )
        .await;
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.contains("malformed"), "{message}");
        if message.contains("malformed-private-secret") {
            leaks.push(format!("field {field}: {message}"));
        }
    }
    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
}

#[tokio::test]
async fn qa_trigger_malformed_receipt_does_not_echo_secret_values() {
    let mut leaks = Vec::new();
    for field in ["run_ids", "replayed"] {
        let server = MockServer::builder().start().await;
        let commit = "a".repeat(64);
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/capabilities"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ordinary_trigger_v1": 1})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut receipt = serde_json::json!({"protocol":"ordinary_trigger_v1","branch":"main","commit_hash":commit,"run_ids":[42],"replayed":false});
        receipt[field] = serde_json::json!("receipt-private-secret");
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/trigger"))
            .respond_with(ResponseTemplate::new(200).set_body_json(receipt))
            .expect(1)
            .mount(&server)
            .await;
        let error = cancel_api(server.uri())
            .trigger_exact("main", &commit, "stable", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not be confirmed"), "{error}");
        if error.contains("receipt-private-secret") {
            leaks.push(format!("field {field}: {error}"));
        }
    }
    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
}

#[tokio::test]
async fn qa_cancel_conflict_refetch_requires_matching_terminal_identity() {
    let commit = "a".repeat(64);
    let mut wrong_id = cancellable_run(43, &commit, "push");
    wrong_id["status"] = serde_json::json!("completed");
    let mut wrong_commit = cancellable_run(42, &"b".repeat(64), "push");
    wrong_commit["status"] = serde_json::json!("completed");
    for followup in [
        ResponseTemplate::new(200).set_body_json(wrong_id),
        ResponseTemplate::new(200).set_body_json(wrong_commit),
        ResponseTemplate::new(200).set_body_string("private-secret"),
        ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
        ResponseTemplate::new(503).set_body_string("private-secret"),
        ResponseTemplate::new(302)
            .insert_header("location", "https://private-secret@example.test/"),
    ] {
        let server = MockServer::builder().start().await;
        let get_count = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/42"))
            .respond_with({
                let get_count = Arc::clone(&get_count);
                let commit = commit.clone();
                move |_: &wiremock::Request| {
                    if get_count.fetch_add(1, Ordering::SeqCst) == 0 {
                        ResponseTemplate::new(200)
                            .set_body_json(cancellable_run(42, &commit, "push"))
                    } else {
                        followup.clone()
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
        let error = cancel_api(server.uri())
            .cancel_exact(42, &commit)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancellation_not_confirmed"), "{error}");
        assert!(!error.contains("private-secret"), "{error}");
    }
}

#[tokio::test]
async fn ci_cancel_cli_uses_existing_exact_endpoint_for_push_and_merge() {
    for event in ["push", "merge"] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        let (repo, commit) = fixture_repo(temp.path(), &server.uri());
        repo.set_metadata(MetadataKey::ApiKey, "test-token")
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/42"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(cancellable_run(42, &commit, event)),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs/42/cancel"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let output = ci_command(
            temp.path(),
            &["cancel", "42", "--commit", &commit, "--json"],
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["schema_version"], 1);
        assert_eq!(receipt["run_id"], 42);
        assert_eq!(receipt["commit_hash"], commit);
        assert_eq!(receipt["event"], event);
        assert_eq!(receipt["outcome"], "control_plane_cancellation_recorded");
        assert_eq!(receipt["control_plane_cancellation_recorded"], true);
        assert_eq!(receipt["execution_stop"], "unconfirmed_best_effort");
        assert!(receipt.get("run").is_none());
    }
}

#[tokio::test]
async fn ci_cancel_json_preflight_failure_is_one_redacted_error_document() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, commit) = fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(cancellable_run(42, &commit, "manual")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("private-secret"))
        .expect(0)
        .mount(&server)
        .await;

    let output = ci_command(
        temp.path(),
        &["cancel", "42", "--commit", &commit, "--json"],
    )
    .await;
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not an ordinary push/merge run"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-secret"));
}

#[tokio::test]
async fn ci_cancel_rejects_ineligible_or_untrusted_preflight_without_posting() {
    let commit = "a".repeat(64);
    let mut terminal = cancellable_run(42, &commit, "push");
    terminal["status"] = serde_json::json!("completed");
    terminal["conclusion"] = serde_json::json!("success");
    let mut wrong_id = cancellable_run(43, &commit, "push");
    wrong_id["id"] = serde_json::json!(43);
    let mut wrong_commit = cancellable_run(42, &commit, "push");
    wrong_commit["commit_hash"] = serde_json::json!("b".repeat(64));
    let mut manual = cancellable_run(42, &commit, "manual");
    let unknown_event = cancellable_run(42, &commit, "scheduled");
    let mut unknown_status = cancellable_run(42, &commit, "push");
    unknown_status["status"] = serde_json::json!("pausing");
    let mut inconsistent = cancellable_run(42, &commit, "push");
    inconsistent["conclusion"] = serde_json::json!("failure");
    // Keep the mutable binding explicit: manual is an independent response,
    // not an alias of a shared JSON value in the table below.
    manual["event"] = serde_json::json!("manual");

    let mut oversized = cancellable_run(42, &commit, "push");
    oversized["jobs"] = serde_json::json!([{"steps":[{"logs":"x".repeat(1024 * 1024)}]}]);

    let cases = vec![
        (
            ResponseTemplate::new(200).set_body_json(terminal),
            "terminal",
        ),
        (
            ResponseTemplate::new(200).set_body_json(wrong_id),
            "identity",
        ),
        (
            ResponseTemplate::new(200).set_body_json(wrong_commit),
            "moved",
        ),
        (ResponseTemplate::new(200).set_body_json(manual), "ordinary"),
        (
            ResponseTemplate::new(200).set_body_json(unknown_event),
            "ordinary",
        ),
        (
            ResponseTemplate::new(200).set_body_json(unknown_status),
            "unknown",
        ),
        (
            ResponseTemplate::new(200).set_body_json(inconsistent),
            "inconsistent",
        ),
        (
            ResponseTemplate::new(200).set_body_string("not-json"),
            "malformed",
        ),
        (ResponseTemplate::new(200).set_body_json(oversized), "1 MiB"),
        (ResponseTemplate::new(404), "may not support CI"),
        (
            ResponseTemplate::new(500).set_body_string("preflight-private-secret"),
            "preflight failed",
        ),
        (
            ResponseTemplate::new(302)
                .insert_header("location", "https://user:redirect-secret@example.test/run"),
            "redirected",
        ),
    ];

    for (response, expected) in cases {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/42"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let error = cancel_api(server.uri())
            .cancel_exact(42, &commit)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
        assert!(!error.contains("redirect-secret"), "{error}");
        assert!(!error.contains("private-secret"), "{error}");
    }
}

#[tokio::test]
async fn ci_cancel_validates_exact_inputs_before_any_http_request() {
    let server = MockServer::builder().start().await;
    let api = cancel_api(server.uri());
    for (run_id, commit) in [
        (0, "a".repeat(64)),
        (i64::MAX as u64 + 1, "a".repeat(64)),
        (42, "abc".into()),
        (42, "A".repeat(64)),
        (42, "z".repeat(64)),
    ] {
        assert!(api.cancel_exact(run_id, &commit).await.is_err());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn ci_cancel_conflict_only_reports_terminal_noop_after_exact_refetch() {
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
                    ResponseTemplate::new(200).set_body_json(cancellable_run(42, &commit, "push"))
                } else {
                    ResponseTemplate::new(200).set_body_json(run_json(
                        42,
                        "main",
                        &commit,
                        "completed",
                        Some("success"),
                    ))
                }
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/42/cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_string("private-secret"))
        .expect(1)
        .mount(&server)
        .await;
    let error = cancel_api(server.uri())
        .cancel_exact(42, &commit)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("terminal no-op confirmed"), "{error}");
    assert!(!error.contains("private-secret"), "{error}");
}

#[tokio::test]
async fn ci_cancel_unknown_conflict_is_unconfirmed_and_never_reposts() {
    let commit = "a".repeat(64);
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/42"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(cancellable_run(42, &commit, "push")),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/42/cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_string("private-secret"))
        .expect(1)
        .mount(&server)
        .await;
    let error = cancel_api(server.uri())
        .cancel_exact(42, &commit)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cancellation_not_confirmed"), "{error}");
    assert!(error.contains("No retry was attempted"), "{error}");
    assert!(!error.contains("private-secret"), "{error}");
}

#[tokio::test]
async fn ci_cancel_mutation_timeout_5xx_redirect_and_malformed_success_are_unconfirmed_and_redacted(
) {
    let commit = "a".repeat(64);
    for response in [
        ResponseTemplate::new(408).set_body_string("timeout-private-secret"),
        ResponseTemplate::new(503).set_body_string("server-private-secret"),
        ResponseTemplate::new(200).set_body_string("malformed-private-secret"),
        ResponseTemplate::new(302).insert_header(
            "location",
            "https://user:redirect-secret@example.test/cancel",
        ),
    ] {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/42"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(cancellable_run(42, &commit, "push")),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs/42/cancel"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let error = cancel_api(server.uri())
            .cancel_exact(42, &commit)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("outcome is unconfirmed"), "{error}");
        assert!(!error.contains("private-secret"), "{error}");
    }
}

#[tokio::test]
async fn ci_cancel_network_loss_after_post_is_unconfirmed_without_retry() {
    let commit = "a".repeat(64);
    let run_body = cancellable_run(42, &commit, "push").to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let (mut get, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let read = get.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET "));
        get.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                run_body.len(), run_body
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        get.shutdown().await.unwrap();

        let (mut post, _) = listener.accept().await.unwrap();
        let read = post.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
        // Drop the accepted connection without any response. The client must
        // report uncertainty and must not open a third connection to retry.
    });
    let error = cancel_api(format!("http://{address}"))
        .cancel_exact(42, &commit)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("outcome is unconfirmed"), "{error}");
    server_task.await.unwrap();
}

#[tokio::test]
async fn ci_trigger_non_success_diagnostics_do_not_echo_remote_secrets() {
    let commit = "a".repeat(64);

    let capability_server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/capabilities"))
        .respond_with(ResponseTemplate::new(500).set_body_string("capability-private-secret"))
        .expect(1)
        .mount(&capability_server)
        .await;
    let error = cancel_api(capability_server.uri())
        .trigger_exact("main", &commit, "stable", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("capability-private-secret"), "{error}");

    let trigger_server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ordinary_trigger_v1":1})),
        )
        .expect(1)
        .mount(&trigger_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/trigger"))
        .respond_with(ResponseTemplate::new(503).set_body_string("trigger-private-secret"))
        .expect(1)
        .mount(&trigger_server)
        .await;
    let error = cancel_api(trigger_server.uri())
        .trigger_exact("main", &commit, "stable", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("trigger-private-secret"), "{error}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_rerun_preserves_every_id_without_claiming_requested_commit_was_verified() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/153"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            153,
            "main",
            "aabbcc",
            "completed",
            Some("failure"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"run_ids":[910,911]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::ci::rerun(temp.path(), 153, true).await;
    let output = oak_cli::output::end_capture();
    result.unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["run_ids"], serde_json::json!([910, 911]));
    assert_eq!(value["run"]["id"], 910);
    assert_eq!(value["requested_commit"], "aabbcc");
    assert_eq!(value["requested_commit_confirmed"], false);
    assert!(!output.contains("logs 0"));
}

#[tokio::test]
async fn ci_rerun_malformed_receipt_values_are_redacted_and_unconfirmed() {
    for receipt in [
        serde_json::json!({"run_ids":"rerun-private-secret"}),
        serde_json::json!({"run_ids":[910],"jobs":"rerun-private-secret"}),
    ] {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(receipt))
            .expect(1)
            .mount(&server)
            .await;
        let api = cancel_api(server.uri());
        let error = api
            .dispatch_run("ci", "main", "aabbcc")
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("rerun-private-secret"), "{error}");
        assert!(error.contains("unconfirmed"), "{error}");
        assert!(error.contains("inspect oak ci runs"), "{error}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn ci_rerun_http_failure_body_and_redirect_are_omitted_without_retry() {
    for status in [302, 307, 400, 403, 408, 409, 429, 500, 503] {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header(
                        "location",
                        format!("{}/rerun-private-location", server.uri()),
                    )
                    .set_body_string("rerun-private-body test-token"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = cancel_api(server.uri())
            .dispatch_run("ci", "main", "aabbcc")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("rerun-private") && !error.contains("test-token"),
            "{error}"
        );
        assert!(
            error.contains("unconfirmed") && error.contains(&status.to_string()),
            "{error}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn ci_rerun_interrupted_post_or_body_omits_request_url_and_never_retries() {
    for partial_response in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut post, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let read = post.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
            if partial_response {
                post.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 999\r\nconnection: close\r\n\r\n{\"run_ids\":").await.unwrap();
            }
            drop(post);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "dispatch retried"
            );
        });
        let api = cancel_api(format!(
            "http://{address}/rerun-private-path?key=rerun-private-query"
        ));
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            api.dispatch_run("ci", "main", "aabbcc"),
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
        server.await.unwrap();
        assert!(error.contains("unconfirmed"), "{error}");
        assert!(
            !error.contains("rerun-private") && !error.contains(&address.to_string()),
            "{error}"
        );
    }
}

#[tokio::test]
async fn ci_rerun_unconfirmed_cli_errors_are_single_safe_json_documents() {
    for response in [
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"run_ids":"rerun-private-secret"})),
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"run_ids":[910],"jobs":"rerun-private-secret"})),
        ResponseTemplate::new(200).set_body_string("{\"rerun-private-secret\": malformed}"),
        ResponseTemplate::new(200).set_body_bytes(vec![255]),
        ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
        ResponseTemplate::new(204),
        ResponseTemplate::new(500).set_body_string("rerun-private-secret"),
        ResponseTemplate::new(307)
            .insert_header("location", "https://rerun-private-secret@example.test/")
            .set_body_string("rerun-private-secret"),
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        fixture_repo(temp.path(), &server.uri());
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/153"))
            .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
                153,
                "main",
                "aabbcc",
                "completed",
                Some("failure"),
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let output = ci_command(temp.path(), &["rerun", "153", "--json"]).await;
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(payload.get("error").is_some() && payload.get("run_ids").is_none());
        let text = payload.to_string();
        assert!(
            text.contains("unconfirmed") && text.contains("inspect oak ci runs"),
            "{text}"
        );
        assert!(!text.contains("rerun-private-secret"), "{text}");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn ci_rerun_invalid_receipts_never_report_run_zero_or_retry() {
    for receipt in [
        serde_json::json!({}),
        serde_json::json!({"run_ids":[]}),
        serde_json::json!({"id":0}),
        serde_json::json!({"run_ids":[910,910]}),
        serde_json::json!({"id":12,"run_ids":[910]}),
    ] {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/ci/runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(receipt))
            .expect(1)
            .mount(&server)
            .await;
        let api = oak_cli::commands::ci::CiClient {
            remote: server.uri(),
            owner: "oak".into(),
            repo: "oak".into(),
            token: None,
        };
        match api.dispatch_run("ci", "main", "aabbcc").await {
            Ok(_) => panic!("invalid receipt accepted"),
            Err(error) => assert!(error.to_string().contains("inspect oak ci runs")),
        }
    }
}

#[tokio::test]
async fn ci_rerun_source_identity_mismatch_never_posts() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/153"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            999,
            "main",
            "aabbcc",
            "completed",
            Some("failure"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    assert!(oak_cli::commands::ci::rerun(temp.path(), 153, false)
        .await
        .is_err());
}

#[tokio::test]
async fn ci_trigger_error_json_and_post_response_budget_are_truthful() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, head) = fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ordinary_trigger_v1":1})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/trigger"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)))
        .expect(1)
        .mount(&server)
        .await;
    let output = ci_command(
        temp.path(),
        &[
            "trigger",
            "--expected-commit",
            &head,
            "--idempotency-key",
            "unchanged",
            "--json",
        ],
    )
    .await;
    assert!(!output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(payload.to_string().contains("same idempotency key"));
    assert!(payload.get("run_ids").is_none());
}

fn assert_native_ci_build_budget(workflow: &str) {
    let cache = workflow
        .split("    cache:\n")
        .nth(1)
        .unwrap()
        .split("    steps:\n")
        .next()
        .unwrap();
    // Restore extracts the complete archive, not the current paths list.
    // An explicit new key must isolate this from the old default `check`
    // cache, including its main-branch fallback.
    assert!(cache.contains("key: cargo-sources-v2"));
    let paths: Vec<_> = cache
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .collect();
    assert_eq!(paths, ["~/.cargo/registry", "~/.cargo/git"]);
    for setting in [
        "CARGO_INCREMENTAL: '0'",
        "CARGO_PROFILE_DEV_DEBUG: '0'",
        "CARGO_PROFILE_TEST_DEBUG: '0'",
    ] {
        assert!(
            workflow.contains(setting),
            "missing native-CI-only setting: {setting}"
        );
    }
    for command in [
        "cargo fmt --all -- --check",
        "cargo clippy --workspace --all-targets -- -D warnings",
        "cargo nextest run --workspace",
        "cargo test --workspace --doc",
    ] {
        assert!(
            workflow
                .lines()
                .any(|line| line.trim() == format!("run: . \"$HOME/.cargo/env\" && {command}")),
            "coverage command changed: {command}"
        );
    }
}

const NATIVE_CI_BUDGET_FIXTURE: &str = r#"on: [push, manual]
env:
  CARGO_INCREMENTAL: '0'
  CARGO_PROFILE_DEV_DEBUG: '0'
  CARGO_PROFILE_TEST_DEBUG: '0'
jobs:
  check:
    cache:
      key: cargo-sources-v2
      paths:
        - ~/.cargo/registry
        - ~/.cargo/git
    steps:
      - name: fmt
        run: . "$HOME/.cargo/env" && cargo fmt --all -- --check
      - name: clippy
        run: . "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings
      - name: test
        run: . "$HOME/.cargo/env" && cargo nextest run --workspace
      - name: doctests
        run: . "$HOME/.cargo/env" && cargo test --workspace --doc
"#;

#[test]
fn native_ci_budget_contract_fixture_accepts_sources_and_rejects_regressions() {
    assert_native_ci_build_budget(NATIVE_CI_BUDGET_FIXTURE);
    for (before, after) in [
        ("key: cargo-sources-v2", "key: check"),
        (
            "        - ~/.cargo/git",
            "        - ~/.cargo/git\n        - target",
        ),
        ("CARGO_INCREMENTAL: '0'", "CARGO_INCREMENTAL: '1'"),
        (
            "CARGO_PROFILE_DEV_DEBUG: '0'",
            "CARGO_PROFILE_DEV_DEBUG: '2'",
        ),
        (
            "CARGO_PROFILE_TEST_DEBUG: '0'",
            "CARGO_PROFILE_TEST_DEBUG: '2'",
        ),
        (
            "cargo nextest run --workspace",
            "cargo nextest run -p oakvcs-core",
        ),
        (
            "cargo test --workspace --doc",
            "cargo test --doc -p oakvcs-core",
        ),
    ] {
        let changed = NATIVE_CI_BUDGET_FIXTURE.replace(before, after);
        assert!(
            std::panic::catch_unwind(|| assert_native_ci_build_budget(&changed)).is_err(),
            "contract accepted regression: {after}"
        );
    }
}

#[test]
fn native_ci_build_budget_preserves_coverage_without_restoring_target() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.oak/workflows/ci.yml");
    match std::fs::read_to_string(&path) {
        Ok(workflow) => assert_native_ci_build_budget(&workflow),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Hosted Git snapshots and Cargo packages omit .oak metadata.
            // The fixture contract above always runs; the actual workflow is
            // additionally checked in a native Oak checkout, never embedded
            // into a test binary built from a source distribution.
            eprintln!("workflow source check unavailable: source distribution omits {}; fixture contract still runs", path.display());
        }
        Err(error) => panic!("could not read {}: {error}", path.display()),
    }
}

/// A minimal on-disk repo that `resolve::resolve` can find (`.oak/oak.db`),
/// on a current branch with one commit, linked to `remote`.
fn fixture_repo(dir: &Path, remote: &str) -> (SqliteRepository, String) {
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
    let head = repo
        .put_commit_and_advance_refs(
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
    (repo, head.to_string())
}

fn run_json(
    id: u64,
    branch: &str,
    commit: &str,
    status: &str,
    conclusion: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "workflow_name": "ci",
        "workflow_path": ".oak/workflows/ci.yml",
        "event": "push",
        "branch": branch,
        "commit_hash": commit,
        "triggered_by": "tester",
        "backend": "sandbox",
        "status": status,
        "conclusion": conclusion,
        "queued_at": "2026-07-07T23:30:27+00:00",
        "started_at": "2026-07-07T23:30:28+00:00",
        "finished_at": if conclusion.is_some() { serde_json::json!("2026-07-07T23:36:50+00:00") } else { serde_json::Value::Null },
    })
}

#[tokio::test(flavor = "current_thread")]
async fn ci_runs_lists_recent_runs() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            run_json(170, "main", "0417051147", "completed", Some("success")),
            run_json(
                153,
                "diff-rename-parity",
                "ec1d378a00",
                "completed",
                Some("failure")
            ),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    oak_cli::commands::ci::runs(temp.path(), 20, false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(captured.contains("#170"), "got: {captured}");
    assert!(captured.contains("success"), "got: {captured}");
    assert!(captured.contains("#153"), "got: {captured}");
    assert!(captured.contains("failure"), "got: {captured}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_status_exit_codes_track_the_merge_gate() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_repo, head) = fixture_repo(temp.path(), &server.uri());

    // Success run for the branch head, plus noise for other commits — the
    // status must pick the head's run (the merge gate's subject), and among
    // several for the head, the newest.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            run_json(171, "tester-ci", &head, "completed", Some("success")),
            run_json(170, "tester-ci", &head, "completed", Some("failure")),
            run_json(169, "other", "ffffffffffff", "completed", Some("failure")),
        ])))
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::status(temp.path(), false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();
    assert_eq!(code, 0, "newest head run concluded success: {captured}");
    assert!(captured.contains("#171"), "got: {captured}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_status_running_and_failure_are_distinct_nonzero() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_repo, head) = fixture_repo(temp.path(), &server.uri());

    let running = ResponseTemplate::new(200).set_body_json(serde_json::json!([run_json(
        172,
        "tester-ci",
        &head,
        "running",
        None
    )]));
    let scope = server
        .register_as_scoped(
            Mock::given(method("GET"))
                .and(path("/api/oak/oak/ci/runs"))
                .respond_with(running),
        )
        .await;
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::status(temp.path(), false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();
    assert_eq!(code, 3, "running CI is exit 3 (retryable): {captured}");
    assert!(
        captured.contains(&format!("oak ci wait 172 --commit {head}")),
        "got: {captured}"
    );
    drop(scope);

    let failed = ResponseTemplate::new(200).set_body_json(serde_json::json!([{
        "id": 153, "workflow_name": "ci", "event": "push", "branch": "tester-ci",
        "commit_hash": head, "status": "completed", "conclusion": "failure",
        "error": "sandbox died (worker redeploy or eviction)",
    }]));
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(failed)
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::status(temp.path(), false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();
    assert_eq!(code, 1, "failed CI is exit 1: {captured}");
    assert!(captured.contains("oak ci logs 153"), "got: {captured}");
    assert!(captured.contains("oak ci rerun 153"), "got: {captured}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_status_json_names_state_and_next_commands() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_repo, head) = fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([run_json(
                171,
                "tester-ci",
                &head,
                "completed",
                Some("success")
            )])),
        )
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::status(temp.path(), true)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(captured.trim()).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["state"], "success");
    assert_eq!(v["branch"], "tester-ci");
    assert_eq!(v["commit"], head);
    assert_eq!(v["run"]["id"], 171);
    assert_eq!(v["recommended_next_commands"][0], "oak merge");
}

/// fb-30: the single-run endpoint may embed raw control characters (ANSI
/// escapes, bare newlines) in step `logs` — invalid strict JSON. `oak ci
/// logs` must recover, not crash.
#[tokio::test(flavor = "current_thread")]
async fn ci_logs_survives_raw_control_chars_in_step_logs() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    // Hand-built body with a raw ESC (0x1b) and raw newline inside the
    // `logs` string — serde_json rejects this outright.
    let body = "{\"id\":153,\"workflow_name\":\"ci\",\"event\":\"push\",\"branch\":\"b\",\
                \"commit_hash\":\"ec1d378a\",\"status\":\"completed\",\"conclusion\":\"failure\",\
                \"error\":\"sandbox died (worker redeploy or eviction)\",\
                \"jobs\":[{\"id\":1,\"name\":\"check\",\"status\":\"completed\",\"conclusion\":\"failure\",\
                \"steps\":[{\"id\":9,\"name\":\"test\",\"status\":\"completed\",\"conclusion\":\"failure\",\
                \"exit_code\":101,\"logs\":\"\u{1b}[31merror[E0308]\u{1b}[0m\nmismatched types\"}]}]}";
    assert!(
        serde_json::from_str::<serde_json::Value>(body).is_err(),
        "fixture must reproduce the invalid-JSON server bug"
    );
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/153"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    oak_cli::commands::ci::logs(temp.path(), 153, false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(captured.contains("sandbox died"), "got: {captured}");
    assert!(captured.contains("check / test"), "got: {captured}");
    assert!(captured.contains("mismatched types"), "got: {captured}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_rerun_dispatches_manual_run_and_reports_new_id() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/153"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            153,
            "diff-rename-parity",
            "ec1d378a00",
            "completed",
            Some("failure"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    // The re-dispatch must carry the old run's coordinates and event=manual.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs"))
        .and(body_partial_json(serde_json::json!({
            "workflow": "ci",
            "branch": "diff-rename-parity",
            "commit": "ec1d378a00",
            "event": "manual",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 171})))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    oak_cli::commands::ci::rerun(temp.path(), 153, false)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(captured.contains("#171"), "got: {captured}");
    assert!(captured.contains("oak ci logs 171"), "got: {captured}");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_rerun_reports_id_from_hosted_run_ids_response() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/673"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            673,
            "main",
            "ec1d378a00",
            "completed",
            Some("failure"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs"))
        .and(body_partial_json(serde_json::json!({
            "workflow": "ci",
            "branch": "main",
            "commit": "ec1d378a00",
            "event": "manual",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"run_ids": [677]})),
        )
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    oak_cli::commands::ci::rerun(temp.path(), 673, true)
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();
    let output: serde_json::Value = serde_json::from_str(&captured).unwrap();

    assert_eq!(output["run"]["id"], 677);
    assert_eq!(output["recommended_next_commands"][1], "oak ci logs 677");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_wait_reports_terminal_state_for_the_requested_run_and_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    let commit = "ec1d378a00";

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            677,
            "main",
            commit,
            "completed",
            Some("success"),
        )))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait(
        temp.path(),
        &[677],
        Some(commit),
        std::time::Duration::from_secs(1),
        true,
    )
    .await
    .unwrap();
    let captured = oak_cli::output::end_capture();
    let output: serde_json::Value = serde_json::from_str(&captured).unwrap();

    assert_eq!(exit, 0);
    assert_eq!(output["state"], "success");
    assert_eq!(output["expected_commit"], commit);
    assert_eq!(output["observations"][0]["requested_run_id"], 677);
    assert_eq!(output["observations"][0]["observed_run_id"], 677);
    assert_eq!(output["observations"][0]["observed_commit"], commit);
}

#[tokio::test]
async fn ci_wait_summary_omits_large_logs_and_preserves_exact_source() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, commit) = fixture_repo(temp.path(), &server.uri());
    let mut run = run_json(677, "tester-ci", &commit, "completed", Some("success"));
    run["jobs"] = serde_json::json!([{"id":1,"name":"build","status":"completed",
        "conclusion":"success","steps":[{"id":2,"name":"test","status":"completed",
        "conclusion":"success","logs":"PRIVATE_LOG_MARKER".repeat(30_000),
        "command":"PRIVATE_SCRIPT_MARKER"}]}]);
    run["provider_secret"] = serde_json::json!("PRIVATE_EXTRA_MARKER");
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run))
        .expect(1)
        .mount(&server)
        .await;

    let result = ci_command(
        temp.path(),
        &[
            "wait",
            "677",
            "--commit",
            &commit,
            "--timeout",
            "0",
            "--summary",
            "--json",
        ],
    )
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.stdout.len() < 4096,
        "summary grew to {} bytes",
        result.stdout.len()
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(!text.contains("PRIVATE_"));
    let receipt: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(receipt["projection"], "summary");
    assert_eq!(receipt["state"], "success");
    assert_eq!(receipt["observations"][0]["observed_commit"], commit);
    assert_eq!(receipt["observations"][0]["observed_run_id"], 677);
    assert_eq!(receipt["observations"][0]["log_bytes_returned"], 0);
    assert!(receipt["observations"][0].get("run").is_none());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn ci_wait_summary_skipped_details_are_not_failures() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, commit) = fixture_repo(temp.path(), &server.uri());
    let mut run = run_json(677, "tester-ci", &commit, "completed", Some("success"));
    run["jobs"] = serde_json::json!([
        {"id":1,"status":"completed","conclusion":"skipped","steps":[]},
        {"id":2,"status":"completed","conclusion":"success","steps":[
            {"id":3,"status":"completed","conclusion":"skipped","exit_code":null}
        ]}
    ]);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run))
        .expect(1)
        .mount(&server)
        .await;
    let result = ci_command(
        temp.path(),
        &["wait", "677", "--summary", "--json", "--timeout", "0"],
    )
    .await;
    assert!(result.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(receipt["observations"][0]["failed_jobs_total"], 0);
    assert_eq!(receipt["observations"][0]["failed_steps_total"], 0);
    assert_eq!(
        receipt["observations"][0]["failure_details"],
        "no_failures_reported"
    );
}

#[tokio::test]
async fn ci_wait_summary_redacts_remote_errors() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(ResponseTemplate::new(503).set_body_string("PRIVATE_REMOTE_ERROR"))
        .expect(1)
        .mount(&server)
        .await;
    let result = ci_command(temp.path(), &["wait", "677", "--summary", "--json"]).await;
    assert_eq!(result.status.code(), Some(6));
    assert!(!String::from_utf8_lossy(&result.stdout).contains("PRIVATE_REMOTE_ERROR"));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE_REMOTE_ERROR"));
}

#[tokio::test]
async fn ci_wait_summary_bounds_failure_details_with_executable_continuation() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let (_, commit) = fixture_repo(temp.path(), &server.uri());
    let mut run = run_json(678, "tester-ci", &commit, "completed", Some("failure"));
    run["jobs"] = serde_json::json!((1..=10).map(|job| serde_json::json!({
        "id":job,"name":"PRIVATE_JOB_NAME".repeat(100),"status":"completed","conclusion":"failure",
        "steps":(1..=10).map(|step| serde_json::json!({"id":step,"status":"completed",
            "conclusion":"failure","exit_code":101,"name":"PRIVATE_STEP_NAME".repeat(100),
            "logs":"PRIVATE_LOG"})).collect::<Vec<_>>()
    })).collect::<Vec<_>>());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/678"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run))
        .expect(2)
        .mount(&server)
        .await;
    let result = ci_command(
        temp.path(),
        &[
            "wait",
            "678",
            "--commit",
            &commit,
            "--timeout",
            "0",
            "--summary",
            "--json",
        ],
    )
    .await;
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.len() < 8192);
    assert!(!String::from_utf8_lossy(&result.stdout).contains("PRIVATE_"));
    let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let observation = &receipt["observations"][0];
    assert_eq!(receipt["state"], "failure");
    assert_eq!(observation["failed_jobs_total"], 10);
    assert_eq!(observation["failed_job_ids"].as_array().unwrap().len(), 8);
    assert_eq!(observation["failed_jobs_omitted"], 2);
    assert_eq!(observation["failed_steps_total"], 100);
    assert_eq!(observation["failed_steps"].as_array().unwrap().len(), 8);
    assert_eq!(observation["failed_steps_omitted"], 92);
    assert!(observation["observed_at"].as_str().is_some());
    let command = observation["details_command"].as_str().unwrap();
    let args: Vec<_> = command
        .strip_prefix("oak ci ")
        .unwrap()
        .split_whitespace()
        .collect();
    let details = ci_command(temp.path(), &args).await;
    assert!(details.status.success());
    let detailed: serde_json::Value = serde_json::from_slice(&details.stdout).unwrap();
    assert_eq!(detailed["run"]["jobs"].as_array().unwrap().len(), 10);
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn ci_wait_summary_preserves_pending_cancelled_and_unavailable_states() {
    for (status, conclusion, exit, state) in [
        ("queued", None, 3, "timeout"),
        ("completed", Some("cancelled"), 1, "failure"),
        ("completed", Some("failure"), 1, "failure"),
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        let (_, commit) = fixture_repo(temp.path(), &server.uri());
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/679"))
            .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
                679,
                "tester-ci",
                &commit,
                status,
                conclusion,
            )))
            .expect(1)
            .mount(&server)
            .await;
        let result = ci_command(
            temp.path(),
            &[
                "wait",
                "679",
                "--commit",
                &commit,
                "--timeout",
                "0",
                "--summary",
                "--json",
            ],
        )
        .await;
        assert_eq!(
            result.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(receipt["state"], state);
        if state == "timeout" {
            let command = receipt["recommended_next_commands"][0].as_str().unwrap();
            assert!(command.contains(&format!("--commit {commit}")));
            assert!(command.contains("--summary"));
            assert!(command.contains("--json"));
        } else {
            assert_eq!(receipt["observations"][0]["failure_details"], "unavailable");
        }
    }
}

#[tokio::test]
async fn ci_wait_summary_rejects_stale_missing_and_malformed_sources() {
    for (status, body) in [
        (404, serde_json::json!({"error":"PRIVATE_ERROR"})),
        (
            200,
            run_json(
                680,
                "tester-ci",
                &"b".repeat(64),
                "completed",
                Some("success"),
            ),
        ),
        (
            200,
            run_json(
                681,
                "tester-ci",
                &"a".repeat(64),
                "completed",
                Some("success"),
            ),
        ),
        (200, serde_json::json!({"id":"PRIVATE_PARSER_VALUE"})),
        (
            200,
            run_json(
                680,
                "tester-ci",
                "PRIVATE_BAD_COMMIT",
                "completed",
                Some("success"),
            ),
        ),
    ] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        fixture_repo(temp.path(), &server.uri());
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/680"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let result = ci_command(
            temp.path(),
            &[
                "wait",
                "680",
                "--commit",
                &"a".repeat(64),
                "--timeout",
                "0",
                "--summary",
                "--json",
            ],
        )
        .await;
        assert_eq!(result.status.code(), Some(6));
        assert!(!String::from_utf8_lossy(&result.stdout).contains("PRIVATE_"));
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE_"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ci_wait_rejects_a_run_from_a_different_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
            677,
            "main",
            "new-head",
            "completed",
            Some("success"),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let error = oak_cli::commands::ci::wait(
        temp.path(),
        &[677],
        Some("expected-head"),
        std::time::Duration::from_secs(1),
        true,
    )
    .await
    .expect_err("a different commit must never satisfy the wait");

    assert!(error.to_string().contains("expected-head"));
    assert!(error.to_string().contains("new-head"));
}

#[tokio::test(flavor = "current_thread")]
async fn ci_wait_aggregates_multiple_exact_runs_and_fails_if_any_failed() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    let commit = "ec1d378a00";

    for (id, conclusion) in [(677, "success"), (678, "failure")] {
        Mock::given(method("GET"))
            .and(path(format!("/api/oak/oak/ci/runs/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(run_json(
                id,
                "main",
                commit,
                "completed",
                Some(conclusion),
            )))
            .expect(1)
            .mount(&server)
            .await;
    }

    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait(
        temp.path(),
        &[677, 678],
        Some(commit),
        std::time::Duration::from_secs(1),
        true,
    )
    .await
    .unwrap();
    let output: serde_json::Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();

    assert_eq!(exit, 1);
    assert_eq!(output["state"], "failure");
    assert_eq!(output["observations"].as_array().unwrap().len(), 2);
    assert_eq!(output["recommended_next_commands"][0], "oak ci logs 678");
}

#[tokio::test(flavor = "current_thread")]
async fn ci_wait_timeout_bounds_network_and_preserves_the_head_fence() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/677"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(2200))
                .set_body_json(run_json(677, "tester-ci", "target", "running", None)),
        )
        .mount(&server)
        .await;

    oak_cli::output::begin_capture();
    let started = std::time::Instant::now();
    let exit = oak_cli::commands::ci::wait(
        temp.path(),
        &[677],
        Some("target"),
        std::time::Duration::from_secs(1),
        true,
    )
    .await
    .unwrap();
    let elapsed = started.elapsed();
    let output: serde_json::Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();

    assert_eq!(exit, 3);
    assert!(
        elapsed < std::time::Duration::from_millis(1700),
        "one-second wait took {elapsed:?}"
    );
    assert_eq!(output["state"], "timeout");
    assert!(output["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .contains("--commit target"));
}
