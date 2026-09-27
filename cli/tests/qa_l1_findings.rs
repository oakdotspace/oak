//! QA-L1 acceptance tests, adopted from independent QA of mrmrs-0a1cc5.
//! Each was RED on d7d2da0a and must stay GREEN.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIAG: &str = "PRIVATE_REMOTE_DIAGNOSTIC sk_live_abc123";

fn oak_env(dir: &Path, home: &Path, args: &[&str], api_key: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null());
    match api_key {
        Some(key) => command.env("OAK_API_KEY", key),
        None => command.env_remove("OAK_API_KEY"),
    };
    command.output().expect("oak binary should run")
}

/// F1: `oak repo list` must not echo raw server error bodies for any status,
/// not only 401/403 (5xx/404 bodies can carry server diagnostics).
#[tokio::test]
async fn qa_repo_list_never_echoes_server_error_bodies() {
    for status in [404u16, 500, 502] {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/repos"))
            .respond_with(ResponseTemplate::new(status).set_body_string(DIAG))
            .mount(&server)
            .await;
        let home = tempfile::TempDir::new().unwrap();
        let uri = server.uri();
        let home_path = home.path().to_path_buf();
        let (json_out, human_out) = tokio::task::spawn_blocking(move || {
            (
                oak_env(
                    &home_path,
                    &home_path,
                    &["repo", "list", "--json", "--remote", &uri],
                    None,
                ),
                oak_env(
                    &home_path,
                    &home_path,
                    &["repo", "list", "--remote", &uri],
                    None,
                ),
            )
        })
        .await
        .unwrap();
        for output in [json_out, human_out] {
            assert!(!output.status.success());
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !all.contains("PRIVATE_REMOTE_DIAGNOSTIC"),
                "HTTP {status} body echoed: {all}"
            );
        }
    }
}

/// F2: when the desc receipt's guidance names `oak push` as a way to sync a
/// failed description, a commit-less `oak push` must actually sync it.
#[tokio::test]
async fn qa_failed_desc_guidance_push_actually_syncs() {
    let repo_dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    oak_cli::commands::init::run(repo_dir.path(), false).unwrap();
    std::fs::write(repo_dir.path().join("kept.txt"), b"work\n").unwrap();
    oak_cli::commands::commit::run(repo_dir.path()).unwrap();
    let data = tempfile::TempDir::new().unwrap();
    let server = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();
    oak_cli::commands::push::run(repo_dir.path(), Some(&server), false, Some("qa/widget"))
        .await
        .unwrap();
    // Point the checkout at a server that rejects metadata (conclusive 409).
    let reject = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&reject)
        .await;
    let db = repo_dir.path().join(".oak/oak.db");
    SqliteRepository::open(&db)
        .unwrap()
        .set_metadata(MetadataKey::RemoteUrl, &reject.uri())
        .unwrap();
    let branch = SqliteRepository::open(&db)
        .unwrap()
        .get_current_branch_name()
        .unwrap()
        .unwrap();
    let dir = repo_dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let receipt = tokio::task::spawn_blocking(move || {
        oak_env(
            &dir,
            &home_path,
            &["desc", "pending narrative", "--json"],
            None,
        )
    })
    .await
    .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&receipt.stdout).unwrap();
    assert_eq!(receipt["remote_synced"], false);
    let guidance = receipt["retry_guidance"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !guidance.contains("oak push") {
        return; // guidance no longer promises push; contract satisfied
    }
    // Restore the real remote and follow the guidance literally.
    SqliteRepository::open(&db)
        .unwrap()
        .set_metadata(MetadataKey::RemoteUrl, &server)
        .unwrap();
    let dir = repo_dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let push = tokio::task::spawn_blocking(move || oak_env(&dir, &home_path, &["push"], None))
        .await
        .unwrap();
    assert!(push.status.success(), "{push:?}");
    let remote: serde_json::Value = reqwest::get(format!("{server}/api/qa/widget/pull"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        remote["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == branch.as_str() && row["description"] == "pending narrative"),
        "guidance said `oak push` syncs it, but the server still has: {}",
        remote["branches"]
    );
}
