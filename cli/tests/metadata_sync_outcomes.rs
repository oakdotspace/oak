//! Public metadata commands retain local success without inventing safe replay.
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use oak_core::{MetadataKey, Repository, SqliteRepository};
use tokio::io::AsyncReadExt;
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

#[tokio::test]
async fn desc_lost_reply_keeps_local_success_and_only_read_only_guidance() {
    let temp = tempfile::TempDir::new().unwrap();
    let data = tempfile::TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("kept.txt"), b"preserve work\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let server = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();
    oak_cli::commands::push::run(temp.path(), Some(&server), false, Some("qa/widget"))
        .await
        .unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    repo.set_metadata(
        MetadataKey::RemoteUrl,
        &format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let upstream = server.clone();
    let forward = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                assert!(bytes.len() < 65536);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            assert!(headers.starts_with("POST /api/qa/widget/push "));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            assert!(length < 65536);
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            assert_eq!(request["branch"]["description"], "new description");
            assert_eq!(request["commits"], serde_json::json!([]));
            let response = reqwest::Client::new()
                .post(format!("{upstream}/api/qa/widget/push"))
                .json(&request)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            let acknowledgement: serde_json::Value = response.json().await.unwrap();
            assert_eq!(acknowledgement["success"], true);
            // Real Serve has committed. Drop the actor's connection without a reply.
            drop(stream);
        })
        .await
        .unwrap();
    });
    let root = temp.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || oak(&root, &["desc", "new description"]))
        .await
        .unwrap();
    forward.await.unwrap();
    assert!(output.status.success());
    assert_eq!(
        repo.get_branch(&branch)
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("new description")
    );
    assert_eq!(repo.get_branch_head(&branch).unwrap(), head);
    assert_eq!(
        std::fs::read(temp.path().join("kept.txt")).unwrap(),
        b"preserve work\n"
    );
    let remote: serde_json::Value = reqwest::get(format!("{server}/api/qa/widget/pull"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(remote["branches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == branch && row["description"] == "new description"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("outcome is unconfirmed"));
    assert!(!stderr.contains("Run `oak push` to retry"), "{stderr}");
    assert!(
        stderr.contains(&format!("oak branch show {branch} --remote --json")),
        "{stderr}"
    );
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
    assert!(stderr.contains("Run `oak push` to retry"));
    assert!(!stderr.contains("outcome is unconfirmed"));
}
