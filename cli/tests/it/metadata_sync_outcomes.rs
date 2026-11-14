//! Public metadata commands retain local success without inventing safe replay.
//!
//! The lost-reply case, which needs a real server, is tested
//! against the real server.
use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_API_KEY", "synthetic-only")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

async fn metadata_reply(close: bool, status: u16) -> (tempfile::TempDir, Output, String) {
    let temp = tempfile::TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let server = MockServer::builder().start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "qa").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();
    Mock::given(method("POST"))
        .and(path("/api/qa/widget/push"))
        .respond_with(ResponseTemplate::new(status).set_body_string("PRIVATE_REMOTE_DIAGNOSTIC"))
        .expect(1)
        .mount(&server)
        .await;
    let root = temp.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        oak(
            &root,
            if close {
                &["close", "--json"]
            } else {
                &["desc", "new description"]
            },
        )
    })
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_REMOTE_DIAGNOSTIC"));
    (temp, output, branch)
}

#[tokio::test]
async fn close_json_uncertainty_keeps_local_receipt_and_read_only_guidance() {
    let (temp, output, branch) = metadata_reply(true, 503).await;
    assert!(output.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["branch"], branch);
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch(&branch).unwrap().unwrap().status,
        oak_core::BranchStatus::Closed
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("outcome is unconfirmed"));
    assert!(!stderr.contains("Run `oak push` to retry"));
    assert!(stderr.contains(&format!("oak branch show {branch} --remote --json")));
}

#[tokio::test]
async fn desc_explicit_rejection_preserves_existing_local_save_and_retry_guidance() {
    let (temp, output, branch) = metadata_reply(false, 403).await;
    assert!(output.status.success());
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch(&branch)
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("new description")
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("was rejected with HTTP 403"));
    // A commit-less push does not send the description; only `oak desc`
    // retries it, after logging in for an auth rejection.
    assert!(!stderr.contains("oak push"), "{stderr}");
    assert!(stderr.contains("Re-run `oak desc` to retry"), "{stderr}");
    assert!(stderr.contains("oak login --remote"), "{stderr}");
    assert!(!stderr.contains("outcome is unconfirmed"));
}
