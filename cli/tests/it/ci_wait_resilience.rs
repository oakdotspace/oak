//! fb-664: `oak ci wait` survives transient read failures inside its original
//! deadline. Only the read-only poll retries; conclusive answers (auth,
//! missing run, malformed body, commit mismatch) never do.
//!
//! Pacing is shortened through the `OAK_CI_WAIT_*` test hooks so no test
//! sleeps real seconds except the ones that assert `Retry-After` itself.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "PRIVATE_REMOTE_ERROR";
const COMMIT: &str = "ec1d378a00";

fn fast_pacing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("OAK_CI_WAIT_POLL_MS", "20");
        std::env::set_var("OAK_CI_WAIT_BACKOFF_BASE_MS", "5");
    });
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
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
}

fn run(id: u64, commit: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": id, "workflow_name": "ci", "workflow_path": ".oak/workflows/ci.yml",
        "event": "push", "branch": "main", "commit_hash": commit,
        "triggered_by": "tester", "backend": "sandbox", "status": status,
        "conclusion": conclusion,
        "queued_at": "2026-07-07T23:30:27+00:00",
        "started_at": "2026-07-07T23:30:28+00:00",
        "finished_at": null,
    })
}

fn run_path(id: u64) -> String {
    format!("/api/oak/oak/ci/runs/{id}")
}

async fn mount_failures(server: &MockServer, id: u64, response: ResponseTemplate, times: u64) {
    Mock::given(method("GET"))
        .and(path(run_path(id)))
        .respond_with(response)
        .up_to_n_times(times)
        .mount(server)
        .await;
}

async fn mount_run(server: &MockServer, body: serde_json::Value) {
    let id = body["id"].as_u64().unwrap();
    Mock::given(method("GET"))
        .and(path(run_path(id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn hits(server: &MockServer, id: u64) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == run_path(id))
        .count()
}

async fn wait(
    dir: &Path,
    ids: &[u64],
    commit: Option<&str>,
    timeout: Duration,
) -> (oak_core::Result<i32>, Duration, String) {
    fast_pacing();
    oak_cli::output::begin_capture();
    let started = Instant::now();
    let result = oak_cli::commands::ci::wait(dir, ids, commit, timeout, true).await;
    let elapsed = started.elapsed();
    (result, elapsed, oak_cli::output::end_capture())
}

fn assert_secret_safe(text: &str) {
    assert!(!text.contains(SECRET), "leaked remote body: {text}");
}

/// A raw HTTP server that drops the first `drop_first` connections without a
/// response (a connection-level failure) and then serves `body` as 200.
async fn flaky_server(drop_first: usize, body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n < drop_first {
                drop(socket);
                continue;
            }
            let body = body.to_string();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, seen)
}

#[tokio::test(flavor = "current_thread")]
async fn transient_connection_failure_then_success_is_observed() {
    let temp = tempfile::TempDir::new().unwrap();
    let (url, seen) = flaky_server(2, run(677, COMMIT, "completed", Some("success"))).await;
    fixture_repo(temp.path(), &url);
    let (result, _, out) = wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
    assert_eq!(result.unwrap(), 0, "{out}");
    assert_eq!(seen.load(Ordering::SeqCst), 3);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["state"], "success");
}

#[tokio::test(flavor = "current_thread")]
async fn http_503_and_429_then_success_are_retried() {
    for status in [503u16, 429, 502, 504, 500] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        fixture_repo(temp.path(), &server.uri());
        mount_failures(
            &server,
            677,
            ResponseTemplate::new(status).set_body_string(SECRET),
            2,
        )
        .await;
        mount_run(&server, run(677, COMMIT, "completed", Some("success"))).await;
        let (result, _, out) =
            wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
        assert_eq!(result.unwrap(), 0, "status {status}: {out}");
        assert_eq!(hits(&server, 677).await, 3, "status {status}");
        assert_secret_safe(&out);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retry_after_is_honored_but_bounded_by_the_deadline() {
    // Honored: a 1s Retry-After delays the retry by at least ~1s.
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(429).insert_header("Retry-After", "1"),
        1,
    )
    .await;
    mount_run(&server, run(677, COMMIT, "completed", Some("success"))).await;
    let (result, elapsed, _) =
        wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
    assert_eq!(result.unwrap(), 0);
    assert!(elapsed >= Duration::from_millis(950), "slept {elapsed:?}");
    assert!(elapsed < Duration::from_millis(3000), "slept {elapsed:?}");

    // Bounded: an hour-long Retry-After never extends the original deadline.
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(503)
            .insert_header("Retry-After", "3600")
            .set_body_string(SECRET),
        100,
    )
    .await;
    let (result, elapsed, out) = wait(
        temp.path(),
        &[677],
        Some(COMMIT),
        Duration::from_millis(600),
    )
    .await;
    let error = result
        .expect_err("unresolved outage must be an error")
        .to_string();
    assert!(error.contains("category=http_503"), "{error}");
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    assert_eq!(hits(&server, 677).await, 1);
    assert_secret_safe(&error);
    assert_secret_safe(&out);
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_http_failure_fails_at_the_deadline_with_a_typed_secret_safe_category() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(503).set_body_string(SECRET),
        1000,
    )
    .await;
    let (result, elapsed, out) = wait(
        temp.path(),
        &[677],
        Some(COMMIT),
        Duration::from_millis(500),
    )
    .await;
    let error = result
        .expect_err("persistent outage must not look like success")
        .to_string();
    assert!(error.contains("category=http_503"), "{error}");
    assert!(error.contains("#677"), "{error}");
    // The suggested next command keeps the wait's output mode (L1).
    assert!(error.contains("`oak ci wait 677 --json`"), "{error}");
    assert!(
        elapsed >= Duration::from_millis(450),
        "gave up early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "overran: {elapsed:?}"
    );
    assert!(
        hits(&server, 677).await >= 3,
        "expected several bounded retries"
    );
    assert_secret_safe(&error);
    assert_secret_safe(&out);
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_connection_failure_fails_at_the_deadline_with_a_typed_category() {
    let temp = tempfile::TempDir::new().unwrap();
    let (url, seen) = flaky_server(usize::MAX, run(677, COMMIT, "running", None)).await;
    fixture_repo(temp.path(), &url);
    let (result, elapsed, _) = wait(
        temp.path(),
        &[677],
        Some(COMMIT),
        Duration::from_millis(500),
    )
    .await;
    let error = result.expect_err("outage must be an error").to_string();
    assert!(error.contains("category=transport_"), "{error}");
    assert!(!error.contains("127.0.0.1"), "URL leaked: {error}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "overran: {elapsed:?}"
    );
    assert!(seen.load(Ordering::SeqCst) >= 3);
}

#[tokio::test(flavor = "current_thread")]
async fn conclusive_answers_are_never_retried() {
    for status in [401u16, 403, 404, 400, 501] {
        let temp = tempfile::TempDir::new().unwrap();
        let server = MockServer::builder().start().await;
        fixture_repo(temp.path(), &server.uri());
        mount_failures(
            &server,
            677,
            ResponseTemplate::new(status).set_body_string("nope"),
            1000,
        )
        .await;
        let (result, elapsed, _) =
            wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
        assert!(result.is_err(), "status {status}");
        assert_eq!(hits(&server, 677).await, 1, "status {status} was retried");
        assert!(
            elapsed < Duration::from_secs(2),
            "status {status}: {elapsed:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_run_is_conclusive() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    // No mock mounted: wiremock answers 404.
    let (result, _, _) = wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("not found"), "{error}");
    assert_eq!(hits(&server, 677).await, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_success_body_is_conclusive_not_transient() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(200).set_body_string("<html>not json</html>"),
        1000,
    )
    .await;
    let (result, _, _) = wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
    assert!(result.is_err());
    assert_eq!(hits(&server, 677).await, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn commit_fence_still_applies_after_recovery() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(&server, 677, ResponseTemplate::new(503), 2).await;
    mount_run(
        &server,
        run(677, "other-head", "completed", Some("success")),
    )
    .await;
    let (result, _, out) = wait(temp.path(), &[677], Some(COMMIT), Duration::from_secs(10)).await;
    let error = result.expect_err("a different commit must never satisfy the wait");
    let error = error.to_string();
    assert!(
        error.contains(COMMIT) && error.contains("other-head"),
        "{error}"
    );
    assert_eq!(hits(&server, 677).await, 3);
    assert!(!out.contains("\"state\": \"success\""));
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_runs_keep_terminal_observations_and_retry_only_the_failing_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_run(&server, run(677, COMMIT, "completed", Some("success"))).await;
    mount_failures(&server, 678, ResponseTemplate::new(503), 2).await;
    mount_run(&server, run(678, COMMIT, "completed", Some("failure"))).await;
    let (result, _, out) = wait(
        temp.path(),
        &[677, 678],
        Some(COMMIT),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(result.unwrap(), 1, "{out}");
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["state"], "failure");
    assert_eq!(doc["observations"].as_array().unwrap().len(), 2);
    // The terminal run is never re-polled; the flaky one took 2 retries.
    assert_eq!(hits(&server, 677).await, 1);
    assert_eq!(hits(&server, 678).await, 3);
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_run_outage_does_not_hide_a_conclusive_failure_in_the_other_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(&server, 677, ResponseTemplate::new(503), 1000).await;
    mount_failures(&server, 678, ResponseTemplate::new(403), 1000).await;
    let (result, elapsed, _) = wait(
        temp.path(),
        &[677, 678],
        Some(COMMIT),
        Duration::from_secs(10),
    )
    .await;
    assert!(result.is_err());
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(hits(&server, 678).await, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn zero_timeout_is_exactly_one_probe_without_retry() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(503).set_body_string(SECRET),
        1000,
    )
    .await;
    let (result, elapsed, out) = wait(temp.path(), &[677], Some(COMMIT), Duration::ZERO).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("category=http_503"), "{error}");
    assert!(error.contains("single probe"), "{error}");
    assert_eq!(hits(&server, 677).await, 1);
    assert!(elapsed < Duration::from_millis(900), "{elapsed:?}");
    assert_secret_safe(&error);
    assert_secret_safe(&out);
}

#[tokio::test(flavor = "current_thread")]
async fn zero_timeout_success_is_still_one_probe() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_run(&server, run(677, COMMIT, "running", None)).await;
    let (result, _, out) = wait(temp.path(), &[677], Some(COMMIT), Duration::ZERO).await;
    assert_eq!(result.unwrap(), 3);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["state"], "timeout");
    assert_eq!(hits(&server, 677).await, 1);
}

/// The real binary: a persistent outage is exactly one JSON error document
/// naming the category, with neither the body nor credentials on any stream.
#[tokio::test(flavor = "current_thread")]
async fn cli_json_reports_the_category_without_body_or_credentials() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_failures(
        &server,
        677,
        ResponseTemplate::new(503).set_body_string(SECRET),
        1000,
    )
    .await;
    let dir = temp.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .current_dir(dir)
            .env("OAK_CI_WAIT_POLL_MS", "20")
            .env("OAK_CI_WAIT_BACKOFF_BASE_MS", "5")
            .args(["ci", "wait", "677", "--json", "--timeout", "1"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(6));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_secret_safe(&stdout);
    assert_secret_safe(&stderr);
    let start = stdout.find('{').expect("one JSON error document");
    let doc: serde_json::Value = serde_json::from_str(&stdout[start..]).unwrap();
    let message = doc["error"]["message"].as_str().unwrap();
    assert!(message.contains("category=http_503"), "{message}");
    assert!(hits(&server, 677).await >= 2);
}
