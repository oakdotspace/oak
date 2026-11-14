//! `oak approve` and the approvals refusal of `oak merge`: what's sent, how a
//! refusal reads, and that nothing waits on a gate waiting can't clear.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HEAD: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn oak(dir: &Path, remote: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("HOME", dir)
        .env("OAK_REMOTE", remote)
        .env("OAK_API_KEY", "oak_reviewkey")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

async fn run(dir: &Path, remote: &str, args: &[&str]) -> Output {
    let (dir, remote) = (dir.to_path_buf(), remote.to_string());
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        oak(&dir, &remote, &args)
    })
    .await
    .unwrap()
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

const TARGET: &[&str] = &["--repo", "acme/company", "--branch", "feature"];
const URL: &str = "/api/acme/company/branches/feature/approvals";

#[tokio::test]
async fn approve_sends_the_boundary_and_reports_what_is_left() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path(URL))
        .and(header("authorization", "Bearer oak_reviewkey"))
        .and(body_partial_json(
            json!({"path": "apps/web", "expected_head": HEAD}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 2,
            "branch": "feature",
            "head": HEAD,
            "enforced": true,
            "boundaries": [
                {"id": 1, "path": "apps/web", "owner_team": "web", "approval": "required", "approved": true},
                {"id": 2, "path": "libs/ui", "owner_team": "design", "approval": "required", "approved": false},
            ],
            "missing_approvals": [
                {"path": "libs/ui", "owner_team": "design", "approvers": ["@group/design"]},
            ],
        })))
        .mount(&server)
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    let mut args = vec!["approve", "--path", "/apps/web/", "--head", HEAD];
    args.extend_from_slice(TARGET);

    let mut with_json = args.clone();
    with_json.push("--json");
    let out = run(temp.path(), &server.uri(), &with_json).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_json(&out)["missing_approvals"][0]["path"], "libs/ui");

    let out = run(temp.path(), &server.uri(), &args).await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("✓ apps/web/"), "{text}");
    assert!(
        text.contains("still needs libs/ui/ (@group/design)"),
        "{text}"
    );
}

/// The capability check answers an unscoped key before the handler, with no
/// `code`; the refusal still says what to do.
#[tokio::test]
async fn an_ordinary_credential_is_told_where_to_approve() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path(URL))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "This API key must explicitly carry the 'review' scope to approve.",
        })))
        .mount(&server)
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    let mut args = vec!["approve", "--json"];
    args.extend_from_slice(TARGET);
    let out = run(temp.path(), &server.uri(), &args).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let body: Value = serde_json::from_slice(&out.stderr)
        .or_else(|_| serde_json::from_slice(&out.stdout))
        .unwrap();
    assert_eq!(body["error"]["code"], "review_key_required");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("'review' scope"), "{message}");
    assert!(
        message.contains("/acme/company/branches/feature"),
        "{message}"
    );
}

#[tokio::test]
async fn self_approval_keeps_its_code_and_a_moved_branch_says_to_pull() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path(URL))
        .and(body_partial_json(json!({"path": "apps/web"})))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "You wrote part of this branch", "code": "self_approval",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(URL))
        .and(body_partial_json(json!({"path": "libs/ui"})))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "Branch 'feature' moved", "code": "branch_moved",
        })))
        .mount(&server)
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    for (boundary, code) in [("apps/web", "self_approval"), ("libs/ui", "branch_moved")] {
        let mut args = vec!["approve", "--json", "--path", boundary];
        args.extend_from_slice(TARGET);
        let out = run(temp.path(), &server.uri(), &args).await;
        let body: Value = serde_json::from_slice(&out.stderr)
            .or_else(|_| serde_json::from_slice(&out.stdout))
            .unwrap();
        assert_eq!(body["error"]["code"], code, "{body}");
        assert_eq!(body["error"]["retryable"], false, "{body}");
    }
}
