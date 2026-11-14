use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_pull_cannot_replace_newer_successfully_published_description() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = Branch::new(
        "feature".into(),
        Some("old description".into()),
        Some("main".into()),
    );
    repo.store_branch(&branch).unwrap();
    repo.set_current_branch("feature").unwrap();
    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    Mock::given(method("GET")).and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(1)).set_body_json(serde_json::json!({
            "head": null, "branch": {"name":"feature", "description":"old description", "parent_branch":"main", "status":"open", "created_at":branch.created_at.to_rfc3339()},
            "branches":[], "commits":[], "blobs":[], "trees":[]
        }))).expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"success":true,"new_head":null,"message":"ok"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let root = temp.path().to_path_buf();
    let remote = server.uri();
    let pull = oak_cli::commands::pull::fetch_current_branch(&root, &remote, false, false);
    let edit = async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !server.received_requests().await.unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        oak_cli::commands::branch::edit_current_branch(temp.path(), "new published description")
            .await
            .unwrap();
        assert!(!repo.branch_description_pending("feature").unwrap());
    };
    let (result, _) = tokio::join!(pull, edit);
    result.unwrap();
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("new published description")
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET")).and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null, "branch": {"name":"feature", "description":"later remote description", "parent_branch":"main", "status":"open", "created_at":branch.created_at.to_rfc3339()},
            "branches":[], "commits":[], "blobs":[], "trees":[]
        }))).expect(1).mount(&server).await;
    oak_cli::commands::pull::fetch_current_branch(&root, &remote, false, false)
        .await
        .unwrap();
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("later remote description")
    );
}
