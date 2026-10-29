//! Actual CLI merge output while the post-merge refresh downloads new content.
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use oak_core::{
    Branch, Commit, FileMode, MetadataKey, Repository, SqliteRepository, Tree, TreeEntry,
    TreeEntryKind,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "qa")
        .env("OAK_API_KEY", "synthetic-test-token")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

async fn merge_with_refresh(json: bool, fail_raw: bool) -> (tempfile::TempDir, Output, String) {
    let temp = tempfile::TempDir::new().unwrap();
    assert!(oak(temp.path(), &["init", "."]).status.success());
    std::fs::write(temp.path().join("tracked.txt"), "before\n").unwrap();
    assert!(oak(temp.path(), &["commit"]).status.success());
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let old = repo.get_branch_head(&branch).unwrap().unwrap();
    repo.store_branch(&Branch::new("main".into(), None, None))
        .unwrap();
    repo.set_branch_head("main", &old).unwrap();
    let content = b"after server merge\n";
    let tree = Tree::new(vec![TreeEntry {
        name: "tracked.txt".into(),
        kind: TreeEntryKind::Blob,
        hash: oak_core::hash_bytes(content),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let merged = Commit::with_timestamp(
        "main".into(),
        Some(old.clone()),
        Some(old.clone()),
        tree.hash.clone(),
        "qa".into(),
        Some("merged remotely".into()),
        Vec::new(),
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(!repo.has_blob(&oak_core::hash_bytes(content)).unwrap());
    assert!(repo.get_manifest(&tree.hash).unwrap().is_none());
    let server = MockServer::builder().start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "qa").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    let landed = Arc::new(AtomicBool::new(false));
    let info_landed = landed.clone();
    let old_head = old.to_string();
    let merged_head = merged.hash.to_string();
    let info_head = merged_head.clone();
    Mock::given(method("GET")).and(path("/api/qa/repo"))
        .respond_with(move |_: &wiremock::Request| ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"head": if info_landed.load(Ordering::SeqCst) { &info_head } else { &old_head }})
        )).expect(2).mount(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/qa/repo/branches/{branch}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head":old.to_string()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let receipt = serde_json::json!({"message":"merged remotely", "commit_hash":merged_head,
        "manifest_hash":tree.hash.to_string(), "parent_hash":old.to_string(), "merge_parent_hash":old.to_string()});
    Mock::given(method("POST"))
        .and(path(format!("/api/qa/repo/branches/{branch}/merge")))
        .respond_with(move |_: &wiremock::Request| {
            landed.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(&receipt)
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/qa/repo/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits":[{"hash":merged.hash.to_string(),"branch_name":"main",
                "parent_hash":old.to_string(),"merge_parent_hash":old.to_string(),
                "manifest_hash":tree.hash.to_string(),"author":"qa","message":"merged remotely",
                "timestamp":merged.timestamp.to_rfc3339(),"files":[]}],
            "trees":[oak_core::protocol::tree_to_wire(&tree)]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/qa/repo/raw/{merged_head}/tracked.txt")))
        .respond_with(if fail_raw {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_bytes(content.to_vec())
        })
        .expect(1)
        .mount(&server)
        .await;
    drop(repo);
    let dir = temp.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        oak(
            &dir,
            if json {
                &["merge", "--json"]
            } else {
                &["merge"]
            },
        )
    })
    .await
    .unwrap();
    (temp, output, merged_head)
}

#[tokio::test]
async fn merge_json_refresh_materializes_files_without_stdout_narration() {
    let (temp, output, merged_head) = merge_with_refresh(true, false).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(temp.path().join("tracked.txt")).unwrap(),
        b"after server merge\n"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout must be one JSON document: {error}; {:?}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(receipt["merged"], true);
    assert_eq!(receipt["commit_hash"], merged_head);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Updating 'main'"));
    assert!(stderr.contains("downloaded 1 new file(s)"));
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_branch_head("main").unwrap().unwrap().to_string(),
        merged_head
    );
}

#[tokio::test]
async fn merge_human_refresh_preserves_stdout_progress() {
    let (temp, output, _) = merge_with_refresh(false, false).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(temp.path().join("tracked.txt")).unwrap(),
        b"after server merge\n"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Updating 'main'"));
    assert!(stdout.contains("downloaded 1 new file(s)"));
}

#[tokio::test]
async fn merge_json_refresh_failure_preserves_server_success_and_json_framing() {
    let (temp, output, merged_head) = merge_with_refresh(true, true).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(temp.path().join("tracked.txt")).unwrap(),
        b"before\n"
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["merged"], true);
    assert_eq!(receipt["commit_hash"], merged_head);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Updating 'main'"));
    assert!(stderr.contains("Couldn't refresh main locally (merge succeeded on server)"));
}
