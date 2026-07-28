//! `oak ci` — the CLI surface over the server's CI runs API.
//!
//! Exercises the real reqwest client against wiremock-served responses:
//! listing runs, the merge-gate status for the current branch head (and its
//! script-facing exit codes), step logs (including the fb-30 shape where the
//! server embeds raw control characters in `logs` — invalid strict JSON that
//! the CLI must parse defensively rather than crash on), and manual re-runs.

use std::path::Path;

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let server = MockServer::start().await;
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
    let server = MockServer::start().await;
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
    let server = MockServer::start().await;
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
    assert!(captured.contains("oak merge --wait"), "got: {captured}");
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
    let server = MockServer::start().await;
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
    let server = MockServer::start().await;
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
    let server = MockServer::start().await;
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
