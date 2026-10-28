//! `oak findings` — the CLI over a branch's review findings.
//!
//! Runs the real binary against wiremock: listing from a checkout (branch and
//! remote from the workspace), posting from "a CI job" with no checkout
//! (`--repo/--branch/--remote` plus `OAK_API_KEY`), and the refusals: a 404
//! from a server with the feature off, and a typed 403 whose code is echoed.
//!
//! Exclusive `MockServer::builder().start()` listeners, as in `ci.rs`.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("oak binary should run")
}

async fn run(dir: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let (dir, home) = (dir.to_owned(), home.to_owned());
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let envs: Vec<(String, String)> = envs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        oak(&dir, &home, &args, &envs)
    })
    .await
    .unwrap()
}

/// A checkout on branch `tester-rf`, linked to `remote` as `oak/oak`.
fn fixture_repo(dir: &Path, remote: &str) -> String {
    let oak_dir = dir.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.store_branch(&Branch::new(
        "tester-rf".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("tester-rf").unwrap();
    let head = repo
        .put_commit_and_advance_refs(
            "tester-rf".to_string(),
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

fn list_body(blocking: i64) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "repo": "oak/oak",
        "branch": "tester-rf",
        "enforced": true,
        "counts": {"open_important": blocking, "open_other": 1, "fixed": 0, "resolved": 0},
        "blocking": blocking,
        "findings": [{
            "id": 7, "display_id": "fd-7", "key": "bug/a", "head": "a".repeat(64),
            "source": "bug", "severity": "important", "state": "open", "blocks_merge": true,
            "title": "Off-by-one in pager",
            "locations": [{"path": "cli/src/pager.rs", "start_line": 10, "end_line": 12}],
            "raised_by": "CI run 4", "revision": 1,
            "created_at": "2026-10-17T00:00:00Z", "updated_at": "2026-10-17T00:00:00Z"
        }]
    })
}

#[tokio::test]
async fn list_from_a_checkout_uses_its_branch_and_names_the_next_command() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-rf/findings"))
        .and(query_param("state", "open"))
        .and(header("authorization", "Bearer key-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_body(1)))
        .expect(1)
        .mount(&server)
        .await;

    let out = run(
        temp.path(),
        home.path(),
        &["findings", "list", "--state", "open", "--json"],
        &[("OAK_API_KEY", "key-1")],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["blocking"], 1);
    assert_eq!(
        json["recommended_next_commands"][0],
        "oak findings list --branch tester-rf --state open --json"
    );

    // Human output names the finding and where it points.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-rf/findings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_body(1)))
        .mount(&server)
        .await;
    let out = run(
        temp.path(),
        home.path(),
        &["findings", "list"],
        &[("OAK_API_KEY", "key-1")],
    )
    .await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("2 open (1 blocking)"), "{text}");
    assert!(text.contains("fd-7"), "{text}");
    assert!(text.contains("cli/src/pager.rs:10-12"), "{text}");
}

#[tokio::test]
async fn post_from_a_ci_job_needs_no_checkout_and_fills_the_reviewed_head() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    let head = "b".repeat(64);
    let file = temp.path().join("findings.json");
    std::fs::write(
        &file,
        serde_json::json!({
            "round": 1,
            "findings": [{"key": "bug/a", "source": "bug", "severity": "important", "title": "t"}],
            "fixed_keys": ["bug/old"]
        })
        .to_string(),
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/acme/web/branches/contrib%2Fbob%2Ffix/findings"))
        .and(header("authorization", "Bearer task-token"))
        .and(body_partial_json(serde_json::json!({
            "expected_head": head,
            "round": 1,
            "fixed_keys": ["bug/old"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "acme/web",
            "branch": "contrib/bob/fix",
            "head": head,
            "findings": [{"id": 9, "key": "bug/a", "disposition": "created"}],
            "fixed": [3],
            "enforced": true,
            "counts": {"open_important": 1, "open_other": 0, "fixed": 1, "resolved": 0},
            "blocking": 1,
            "recommended_next_commands": ["oak findings list --branch contrib/bob/fix --state open --json"]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let out = run(
        temp.path(),
        home.path(),
        &[
            "findings",
            "post",
            "--repo",
            "acme/web",
            "--branch",
            "contrib/bob/fix",
            "--remote",
            &server.uri(),
            "--expected-head",
            &head,
            "--file",
            file.to_str().unwrap(),
        ],
        &[("OAK_API_KEY", "task-token")],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("1 new, 0 reopened, 0 already known, 1 marked fixed; 1 blocking"),
        "{text}"
    );
}

#[tokio::test]
async fn a_server_without_the_feature_gets_an_explained_json_error() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/web/branches/feature/findings"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "Not found"})),
        )
        .mount(&server)
        .await;

    let out = run(
        temp.path(),
        home.path(),
        &[
            "findings",
            "list",
            "--repo",
            "acme/web",
            "--branch",
            "feature",
            "--remote",
            &server.uri(),
            "--json",
        ],
        &[],
    )
    .await;
    assert_eq!(out.status.code(), Some(6));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON envelope");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("not enabled for this repository"),
        "{message}"
    );
}

#[tokio::test]
async fn a_typed_refusal_echoes_the_servers_message_and_code_only() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let server = MockServer::builder().start().await;
    fixture_repo(temp.path(), &server.uri());
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/branches/tester-rf/findings/3/resolve"))
        .and(body_partial_json(
            serde_json::json!({"reason": "wrong", "expected_revision": 2}),
        ))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "fd-3 is a security finding; a person resolves it from the branch page",
            "code": "human_resolution_required",
            "secret": "never printed"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let out = run(
        temp.path(),
        home.path(),
        &[
            "findings",
            "resolve",
            "fd-3",
            "--reason",
            "wrong",
            "--expected-revision",
            "2",
        ],
        &[("OAK_API_KEY", "key-1")],
    )
    .await;
    assert_eq!(out.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("human_resolution_required"), "{stderr}");
    assert!(stderr.contains("a person resolves it"), "{stderr}");
    assert!(!stderr.contains("never printed"), "{stderr}");

    // Bad input never reaches the server.
    let out = run(
        temp.path(),
        home.path(),
        &["findings", "resolve", "fd-3", "--reason", "because"],
        &[("OAK_API_KEY", "key-1")],
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
