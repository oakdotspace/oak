//! `oak ci wait --current`, opt-in wait progress, `run_url` in CI JSON, and
//! `oak ci cancel --superseded` (fb-326/352/354/358/406/433/472).
//!
//! Exercises the real reqwest client against exclusive wiremock listeners
//! (see the note in `ci.rs` about pooled listeners and `DispatchGone`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture_repo(dir: &Path, remote: &str) -> String {
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
    head.to_string()
}

fn run(
    id: u64,
    branch: &str,
    commit: &str,
    event: &str,
    workflow: &str,
    status: &str,
    conclusion: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "workflow_name": workflow,
        "workflow_path": format!(".oak/workflows/{workflow}.yml"),
        "event": event,
        "branch": branch,
        "commit_hash": commit,
        "triggered_by": "tester",
        "backend": "sandbox",
        "status": status,
        "conclusion": conclusion,
        "queued_at": "2026-09-26T10:00:00+00:00",
        "started_at": "2026-09-26T10:00:01+00:00",
        "finished_at": if conclusion.is_some() { json!("2026-09-26T10:05:00+00:00") } else { Value::Null },
    })
}

fn with_steps(mut run: Value, steps: &[(&str, &str, Option<&str>)]) -> Value {
    run["jobs"] = json!([{
        "id": 1,
        "name": "check",
        "status": run["status"].clone(),
        "conclusion": run["conclusion"].clone(),
        "steps": steps.iter().enumerate().map(|(i, (name, status, conclusion))| json!({
            "id": 100 + i as u64,
            "name": name,
            "command": "secret script body",
            "status": status,
            "conclusion": conclusion,
            "exit_code": conclusion.map(|c| if c == "success" { 0 } else { 1 }),
            "logs": "LOG-TEXT-MUST-NOT-APPEAR-IN-PROGRESS",
        })).collect::<Vec<_>>(),
    }]);
    run
}

async fn mount_runs(server: &MockServer, runs: Value) {
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(runs))
        .mount(server)
        .await;
}

async fn mount_run(server: &MockServer, id: u64, body: Value, expect: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/ci/runs/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(expect)
        .mount(server)
        .await;
}

async fn mount_remote_head(server: &MockServer, head: Option<&str>) {
    let template = match head {
        Some(head) => ResponseTemplate::new(200).set_body_json(json!({ "head": head })),
        None => ResponseTemplate::new(404).set_body_json(json!({ "error": "not found" })),
    };
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-ci"))
        .respond_with(template)
        .mount(server)
        .await;
}

async fn oak(dir: &Path, args: &[&str]) -> std::process::Output {
    let dir = dir.to_owned();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .current_dir(dir)
            .args(args)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_API_KEY")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn opts(timeout: u64, summary: bool) -> oak_cli::commands::ci::WaitOptions {
    oak_cli::commands::ci::WaitOptions {
        timeout: std::time::Duration::from_secs(timeout),
        json: true,
        summary,
        progress: oak_cli::commands::ci::ProgressMode::Off,
    }
}

/// The head is resolved once and bound to the newest run per workflow for
/// exactly that commit (the merge gate's view); an older rerun of the same
/// workflow and runs for other commits are never polled.
#[tokio::test(flavor = "current_thread")]
async fn current_binds_newest_run_per_workflow_for_exact_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let other = "e".repeat(64);
    mount_runs(
        &server,
        json!([
            run(14, "tester-ci", &other, "push", "ci", "running", None),
            run(
                13,
                "tester-ci",
                &head,
                "push",
                "lint",
                "completed",
                Some("success")
            ),
            run(
                12,
                "tester-ci",
                &head,
                "manual",
                "ci",
                "completed",
                Some("success")
            ),
            run(
                11,
                "tester-ci",
                &head,
                "push",
                "ci",
                "completed",
                Some("failure")
            ),
        ]),
    )
    .await;
    for id in [12, 13] {
        let (workflow, event) = if id == 12 {
            ("ci", "manual")
        } else {
            ("lint", "push")
        };
        mount_run(
            &server,
            id,
            run(
                id,
                "tester-ci",
                &head,
                event,
                workflow,
                "completed",
                Some("success"),
            ),
            1,
        )
        .await;
    }
    mount_run(&server, 11, json!({}), 0).await;
    mount_run(&server, 14, json!({}), 0).await;

    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait_current(
        temp.path(),
        opts(5, false),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(exit, 0, "{v}");
    assert_eq!(v["state"], "success");
    assert_eq!(v["requested_run_ids"], json!([12, 13]));
    assert_eq!(v["expected_commit"], head);
    assert_eq!(v["subject"], "current_head");
    assert_eq!(v["current_branch"], "tester-ci");
    assert_eq!(
        v["binding"],
        "latest_run_per_workflow_for_exact_commit_on_current_branch"
    );
    assert!(v.get("bound_branches").is_none());
    assert_eq!(
        v["observations"][0]["run_url"],
        format!("{}/oak/oak/ci/runs/12", server.uri())
    );
    assert_eq!(v["recommended_next_commands"], json!(["oak merge"]));
}

/// A failed newest run for the head fails the wait (exit 1) exactly like an
/// exact-run wait, and summary mode keeps its bounded projection.
#[tokio::test(flavor = "current_thread")]
async fn current_summary_failure_uses_existing_exit_codes() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    mount_runs(
        &server,
        json!([run(
            30,
            "tester-ci",
            &head,
            "push",
            "ci",
            "completed",
            Some("failure")
        )]),
    )
    .await;
    mount_run(
        &server,
        30,
        with_steps(
            run(
                30,
                "tester-ci",
                &head,
                "push",
                "ci",
                "completed",
                Some("failure"),
            ),
            &[
                ("build", "completed", Some("success")),
                ("test", "completed", Some("failure")),
            ],
        ),
        1,
    )
    .await;
    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait_current(
        temp.path(),
        opts(5, true),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let text = oak_cli::output::end_capture();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(exit, 1);
    assert_eq!(v["projection"], "summary");
    assert_eq!(v["state"], "failure");
    assert_eq!(v["observations"][0]["failed_steps"][0]["step_id"], 101);
    assert_eq!(
        v["observations"][0]["run_url"],
        format!("{}/oak/oak/ci/runs/30", server.uri())
    );
    assert!(!text.contains("LOG-TEXT"), "{text}");
}

/// No run for the exact head within the dispatch bound: report
/// `dispatch_pending` (exit 3, retryable) and never bind to a run for an
/// older commit on the same branch.
#[tokio::test(flavor = "current_thread")]
async fn current_dispatch_pending_never_binds_an_older_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let older = "c".repeat(64);
    mount_runs(
        &server,
        json!([run(40, "tester-ci", &older, "push", "ci", "running", None)]),
    )
    .await;
    mount_run(&server, 40, json!({}), 0).await;
    mount_remote_head(&server, Some(&head)).await;

    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait_current(
        temp.path(),
        opts(30, false),
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(exit, 3, "{v}");
    assert_eq!(v["state"], "dispatch_pending");
    assert_eq!(v["requested_run_ids"], json!([]));
    assert_eq!(v["observations"], json!([]));
    assert_eq!(v["expected_commit"], head);
    assert_eq!(v["head_published"], true);
    assert_eq!(v["remote_branch_head"], head);
    assert_eq!(
        v["recommended_next_commands"][0],
        "oak ci wait --current --json"
    );
}

/// An unpublished head is reported as such, with `oak push` first.
#[tokio::test(flavor = "current_thread")]
async fn current_dispatch_pending_reports_unpublished_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_runs(&server, json!([])).await;
    mount_remote_head(&server, None).await;
    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait_current(
        temp.path(),
        opts(0, true),
        std::time::Duration::from_secs(120),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(exit, 3);
    assert_eq!(v["state"], "dispatch_pending");
    assert_eq!(v["projection"], "summary");
    assert_eq!(v["head_published"], false);
    assert!(v.get("remote_branch_head").is_none());
    assert_eq!(v["recommended_next_commands"][0], "oak push --json");
    assert_eq!(
        v["recommended_next_commands"][1],
        "oak ci wait --current --json --summary"
    );
}

/// End to end through the binary: dispatch arrives after a bounded wait,
/// progress events are JSONL on stderr derived from step metadata only (no
/// log text or scripts), and stdout is exactly one final JSON document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_waits_for_dispatch_and_streams_bounded_step_events() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    // First list: nothing yet. Afterwards: the head's run.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_runs(
        &server,
        json!([run(50, "tester-ci", &head, "push", "ci", "queued", None)]),
    )
    .await;
    let poll = |status: &str, conclusion: Option<&str>, steps: &[(&str, &str, Option<&str>)]| {
        with_steps(
            run(50, "tester-ci", &head, "push", "ci", status, conclusion),
            steps,
        )
    };
    for body in [
        poll(
            "running",
            None,
            &[("build", "running", None), ("test", "queued", None)],
        ),
        poll(
            "running",
            None,
            &[
                ("build", "completed", Some("success")),
                ("test", "running", None),
            ],
        ),
    ] {
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/ci/runs/50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    mount_run(
        &server,
        50,
        poll(
            "completed",
            Some("success"),
            &[
                ("build", "completed", Some("success")),
                ("test", "completed", Some("success")),
            ],
        ),
        1,
    )
    .await;

    let out = oak(
        temp.path(),
        &[
            "ci",
            "wait",
            "--current",
            "--json",
            "--events",
            "--timeout",
            "30",
            "--dispatch-timeout",
            "20",
        ],
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["state"], "success");
    assert_eq!(doc["requested_run_ids"], json!([50]));
    assert!(doc["dispatch_waited_ms"].as_u64().unwrap() >= 1000, "{doc}");

    assert!(!stderr.contains("LOG-TEXT"), "{stderr}");
    assert!(!stderr.contains("secret script"), "{stderr}");
    let events: Vec<Value> = stderr
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("JSONL: {line}")))
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "dispatch_pending", "{kinds:?}");
    assert_eq!(kinds[1], "bound", "{kinds:?}");
    let steps: Vec<(u64, &str)> = events
        .iter()
        .filter(|e| e["event"] == "step")
        .map(|e| {
            (
                e["step_id"].as_u64().unwrap(),
                e["conclusion"]
                    .as_str()
                    .unwrap_or_else(|| e["status"].as_str().unwrap()),
            )
        })
        .collect();
    assert_eq!(
        steps,
        vec![
            (100, "running"),
            (100, "success"),
            (101, "running"),
            (101, "success")
        ],
        "one event per transition: {stderr}"
    );
    let last = events.last().unwrap();
    assert_eq!(last["event"], "run");
    assert_eq!(last["gate_state"], "success");
    assert!(events.iter().all(|e| e.get("tests").is_none()));
}

/// Text progress is opt-in; without the flag stderr carries no progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_is_opt_in_and_text_mode_is_human_readable() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs/60"))
        .respond_with(ResponseTemplate::new(200).set_body_json(with_steps(
            run(
                60,
                "tester-ci",
                &head,
                "push",
                "ci",
                "completed",
                Some("success"),
            ),
            &[("build", "completed", Some("success"))],
        )))
        .mount(&server)
        .await;
    let args = [
        "ci",
        "wait",
        "60",
        "--commit",
        &head,
        "--json",
        "--timeout",
        "5",
    ];
    let quiet = oak(temp.path(), &args).await;
    assert_eq!(quiet.status.code(), Some(0));
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("ci progress"),
        "{}",
        String::from_utf8_lossy(&quiet.stderr)
    );
    let quiet_doc: Value = serde_json::from_slice(&quiet.stdout).unwrap();
    assert_eq!(
        quiet_doc["observations"][0]["run_url"],
        format!("{}/oak/oak/ci/runs/60", server.uri())
    );

    let mut with_progress = args.to_vec();
    with_progress.push("--progress");
    let loud = oak(temp.path(), &with_progress).await;
    assert_eq!(loud.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&loud.stderr);
    assert!(stderr.contains("ci progress"), "{stderr}");
    assert!(stderr.contains("run #60 success"), "{stderr}");
    let loud_doc: Value = serde_json::from_slice(&loud.stdout).unwrap();
    assert_eq!(loud_doc["state"], quiet_doc["state"]);
    assert_eq!(
        loud_doc["requested_run_ids"],
        quiet_doc["requested_run_ids"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_and_dispatch_flags_are_validated_before_network() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    for args in [
        vec!["ci", "wait", "5", "--dispatch-timeout", "3"],
        vec!["ci", "wait", "--current", "5"],
        vec!["ci", "wait", "--current", "--commit", &"a".repeat(64)],
        vec!["ci", "cancel", "5", "--commit", &"a".repeat(64), "--yes"],
        vec!["ci", "cancel", "--superseded", "--dry-run", "--yes"],
    ] {
        let out = oak(temp.path(), &args).await;
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn runs_json_carries_canonical_run_url() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let mut preset = run(71, "tester-ci", &head, "push", "ci", "running", None);
    preset["run_url"] = json!("https://example.invalid/kept");
    mount_runs(
        &server,
        json!([
            run(72, "tester-ci", &head, "push", "ci", "running", None),
            preset
        ]),
    )
    .await;
    oak_cli::output::begin_capture();
    oak_cli::commands::ci::runs(temp.path(), 5, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(
        v["runs"][0]["run_url"],
        format!("{}/oak/oak/ci/runs/72", server.uri())
    );
    assert_eq!(v["runs"][1]["run_url"], "https://example.invalid/kept");
}

/// Branch runs for `--superseded`: head run 20; older push runs 18 (queued)
/// and 17 (running) at older commits; a newer push run 21 at another commit
/// (e.g. from a concurrent pusher); a manual run, a merge run, a completed
/// push, and another branch's push that must never be candidates.
async fn mount_superseded_fixture(server: &MockServer, head: &str) -> (String, String) {
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    mount_runs(
        server,
        json!([
            run(22, "other-branch", &a, "push", "ci", "running", None),
            run(
                21,
                "tester-ci",
                &"d".repeat(64),
                "push",
                "ci",
                "running",
                None
            ),
            run(20, "tester-ci", head, "push", "ci", "running", None),
            run(19, "tester-ci", &a, "manual", "ci", "running", None),
            run(18, "tester-ci", &a, "push", "ci", "queued", None),
            run(17, "tester-ci", &b, "push", "ci", "running", None),
            run(16, "tester-ci", &b, "merge", "ci", "running", None),
            run(
                15,
                "tester-ci",
                &b,
                "push",
                "ci",
                "completed",
                Some("failure")
            ),
        ]),
    )
    .await;
    (a, b)
}

async fn mount_cancel(server: &MockServer, id: u64, expect: u64) {
    Mock::given(method("POST"))
        .and(path(format!("/api/oak/oak/ci/runs/{id}/cancel")))
        .respond_with(ResponseTemplate::new(204))
        .expect(expect)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_dry_run_lists_only_older_same_branch_push_runs() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    mount_superseded_fixture(&server, &head).await;
    mount_remote_head(&server, Some(&head)).await;
    for id in [15, 16, 17, 18, 19, 20, 21, 22] {
        mount_cancel(&server, id, 0).await;
    }
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), false, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["head_run_ids"], json!([20]));
    assert!(v.get("blocked_reason").is_none(), "{v}");
    let got: Vec<(u64, &str)> = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["run_id"].as_u64().unwrap(), c["action"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![(17, "would_cancel"), (18, "would_cancel"), (21, "skipped")]
    );
    assert_eq!(v["candidates"][2]["skip_reason"], "newer_than_head_run");
    assert_eq!(
        v["candidates"][0]["run_url"],
        format!("{}/oak/oak/ci/runs/17", server.uri())
    );
    assert_eq!(
        v["recommended_next_commands"][0],
        "oak ci cancel --superseded --yes --json"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_yes_cancels_through_the_exact_run_path_only() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let (a, b) = mount_superseded_fixture(&server, &head).await;
    mount_remote_head(&server, Some(&head)).await;
    // Exact-run preflight: 18 still queued at its commit; 17's record now
    // reports a manual event, so the exact path must refuse it.
    mount_run(
        &server,
        18,
        run(18, "tester-ci", &a, "push", "ci", "queued", None),
        1,
    )
    .await;
    mount_run(
        &server,
        17,
        run(17, "tester-ci", &b, "manual", "ci", "running", None),
        1,
    )
    .await;
    mount_cancel(&server, 18, 1).await;
    for id in [15, 16, 17, 19, 20, 21, 22] {
        mount_cancel(&server, id, 0).await;
    }
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), true, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(code, 1, "a refused cancellation is reported: {v}");
    assert_eq!(v["dry_run"], false);
    let c = v["candidates"].as_array().unwrap();
    assert_eq!(c[0]["run_id"], 17);
    assert_eq!(c[0]["action"], "not_cancelled");
    assert!(c[0]["error"]
        .as_str()
        .unwrap()
        .contains("not an ordinary push/merge run"));
    assert_eq!(c[1]["run_id"], 18);
    assert_eq!(c[1]["action"], "cancelled");
    assert_eq!(c[1]["execution_stop"], "unconfirmed_best_effort");
    assert_eq!(c[2]["action"], "skipped");
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_fails_closed_when_checkout_is_not_the_published_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    mount_superseded_fixture(&server, &head).await;
    // Someone else published a newer head: this checkout is stale.
    mount_remote_head(&server, Some(&"d".repeat(64))).await;
    for id in [15, 16, 17, 18, 19, 20, 21, 22] {
        mount_cancel(&server, id, 0).await;
    }
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), true, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(code, 1);
    assert_eq!(
        v["blocked_reason"],
        "checkout_head_is_not_remote_branch_head"
    );
    assert!(v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["action"] == "skipped" && c["eligible"] == false));
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_requires_an_observed_head_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    mount_runs(
        &server,
        json!([run(
            5,
            "tester-ci",
            &"a".repeat(64),
            "push",
            "ci",
            "running",
            None
        )]),
    )
    .await;
    mount_remote_head(&server, Some(&head)).await;
    mount_cancel(&server, 5, 0).await;
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), false, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(code, 0, "dry run only reports");
    assert_eq!(v["blocked_reason"], "no_run_observed_for_head");
    assert_eq!(v["candidates"][0]["action"], "skipped");
    assert_eq!(
        v["recommended_next_commands"],
        json!(["oak ci wait --current --json"])
    );
}

/// QA M1: a same-commit run on another branch (here a stale failure) is not
/// bound while this branch's own run may still be dispatched; the branch's
/// own run wins once it appears.
#[tokio::test(flavor = "current_thread")]
async fn current_prefers_this_branch_over_same_commit_runs_elsewhere() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let stale = run(
        30,
        "other-branch",
        &head,
        "push",
        "ci",
        "completed",
        Some("failure"),
    );
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([stale.clone()])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_runs(
        &server,
        json!([
            run(31, "tester-ci", &head, "push", "ci", "queued", None),
            stale
        ]),
    )
    .await;
    mount_run(
        &server,
        31,
        run(
            31,
            "tester-ci",
            &head,
            "push",
            "ci",
            "completed",
            Some("success"),
        ),
        1,
    )
    .await;
    mount_run(&server, 30, json!({}), 0).await;
    oak_cli::output::begin_capture();
    let exit = oak_cli::commands::ci::wait_current(
        temp.path(),
        opts(20, false),
        std::time::Duration::from_secs(15),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(exit, 0, "{v}");
    assert_eq!(v["requested_run_ids"], json!([31]));
    assert_eq!(
        v["binding"],
        "latest_run_per_workflow_for_exact_commit_on_current_branch"
    );
}

/// QA M1: when only another branch has runs for the exact commit after the
/// dispatch bound, they are bound but labelled explicitly.
#[tokio::test(flavor = "current_thread")]
async fn current_labels_other_branch_same_commit_binding() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    mount_runs(
        &server,
        json!([run(
            30,
            "main",
            &head,
            "merge",
            "ci",
            "completed",
            Some("failure")
        )]),
    )
    .await;
    mount_run(
        &server,
        30,
        run(
            30,
            "main",
            &head,
            "merge",
            "ci",
            "completed",
            Some("failure"),
        ),
        1,
    )
    .await;
    oak_cli::output::begin_capture();
    let exit =
        oak_cli::commands::ci::wait_current(temp.path(), opts(5, true), std::time::Duration::ZERO)
            .await
            .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(exit, 1);
    assert_eq!(v["binding"], "other_branch_same_commit");
    assert_eq!(v["bound_branches"], json!(["main"]));
    assert_eq!(v["current_branch"], "tester-ci");
}

/// QA L1: `--timeout 0` stays a ~1s probe even if the diagnostic remote
/// branch-head read is slow; the head's publication is then unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_timeout_zero_bounds_the_remote_head_diagnostic() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    mount_runs(&server, json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-ci"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(8))
                .set_body_json(json!({"head": "a".repeat(64)})),
        )
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let out = oak(
        temp.path(),
        &["ci", "wait", "--current", "--json", "--timeout", "0"],
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "took {elapsed:?}"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "dispatch_pending");
    assert!(v.get("head_published").is_none(), "{v}");
}

/// QA M2: HTTP 500 after the cancel POST is an unknown outcome, reported as
/// `outcome_unknown` (not a known `not_cancelled`), with no retry.
#[tokio::test(flavor = "current_thread")]
async fn superseded_http_500_is_outcome_unknown_without_retry() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let a = "a".repeat(64);
    mount_runs(
        &server,
        json!([
            run(20, "tester-ci", &head, "push", "ci", "running", None),
            run(18, "tester-ci", &a, "push", "ci", "queued", None),
        ]),
    )
    .await;
    mount_remote_head(&server, Some(&head)).await;
    mount_run(
        &server,
        18,
        run(18, "tester-ci", &a, "push", "ci", "queued", None),
        1,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/18/cancel"))
        .respond_with(ResponseTemplate::new(500).set_body_string("secret-ish body"))
        .expect(1)
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), true, true)
        .await
        .unwrap();
    let text = oak_cli::output::end_capture();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(code, 1);
    assert_eq!(v["candidates"][0]["run_id"], 18);
    assert_eq!(v["candidates"][0]["action"], "outcome_unknown");
    assert!(v["candidates"][0]["error"]
        .as_str()
        .unwrap()
        .contains("unconfirmed"));
    assert!(!text.contains("secret-ish"), "{text}");
}

/// A definitive server refusal (4xx) stays `not_cancelled`.
#[tokio::test(flavor = "current_thread")]
async fn superseded_http_403_is_a_known_refusal() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = fixture_repo(temp.path(), &server.uri());
    let a = "a".repeat(64);
    mount_runs(
        &server,
        json!([
            run(20, "tester-ci", &head, "push", "ci", "running", None),
            run(18, "tester-ci", &a, "push", "ci", "queued", None),
        ]),
    )
    .await;
    mount_remote_head(&server, Some(&head)).await;
    mount_run(
        &server,
        18,
        run(18, "tester-ci", &a, "push", "ci", "queued", None),
        1,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/ci/runs/18/cancel"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), true, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    assert_eq!(code, 1);
    assert_eq!(v["candidates"][0]["action"], "not_cancelled");
}

/// QA M2: the connection drops after the cancel POST is sent. The outcome is
/// unknown, reported as `outcome_unknown`, and the POST is never retried.
#[tokio::test(flavor = "current_thread")]
async fn superseded_network_drop_after_post_is_outcome_unknown_without_retry() {
    let temp = tempfile::TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote = format!("http://{}", listener.local_addr().unwrap());
    let head = fixture_repo(temp.path(), &remote);
    let a = "a".repeat(64);
    let list = json!([
        run(20, "tester-ci", &head, "push", "ci", "running", None),
        run(18, "tester-ci", &a, "push", "ci", "queued", None),
    ])
    .to_string();
    let detail = run(18, "tester-ci", &a, "push", "ci", "queued", None).to_string();
    let branch = json!({ "head": head }).to_string();
    let posts = Arc::new(AtomicUsize::new(0));
    let server_posts = posts.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0; 8192];
            let read = conn.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let line = request.lines().next().unwrap_or_default().to_string();
            if line.starts_with("POST ") {
                server_posts.fetch_add(1, Ordering::SeqCst);
                drop(conn); // no response at all
                continue;
            }
            let body = if line.contains("/branches/") {
                &branch
            } else if line.contains("/ci/runs/18") {
                &detail
            } else {
                &list
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = conn.write_all(response.as_bytes()).await;
            let _ = conn.shutdown().await;
        }
    });
    oak_cli::output::begin_capture();
    let code = oak_cli::commands::ci::cancel_superseded(temp.path(), true, true)
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&oak_cli::output::end_capture()).unwrap();
    server.abort();
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["candidates"][0]["run_id"], 18);
    assert_eq!(v["candidates"][0]["action"], "outcome_unknown");
    assert_eq!(posts.load(Ordering::SeqCst), 1, "never retried");
}
