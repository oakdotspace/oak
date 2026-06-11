use chrono::Utc;
use oak_cli::output;
use oak_core::{
    Branch, ChangeType, FileChange, FileMode, ManifestEntry, Repository, SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn seed_one_commit(repo: &SqliteRepository, branch_name: &str) {
    let branch = Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    );
    repo.store_branch(&branch).unwrap();
    repo.set_current_branch(branch_name).unwrap();

    let blob_hash = repo.put_blob(b"hello world\n".to_vec()).unwrap();
    let manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "hello.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let commit_hash = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "hello.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &commit_hash).unwrap();
    repo.set_head(&commit_hash).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_output_includes_branch_url() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    assert!(captured.contains(&format!(
        "Push complete: {}/oak/oak/branches/{branch_name}",
        server.uri()
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_without_branch_keeps_plain_output() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    assert!(captured.contains("Push complete"));
    assert!(!captured.contains("/branches/"));
}
