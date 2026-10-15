use oak_core::{
    Branch, BranchStatus, Commit, FileMode, Manifest, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn commit(
    repo: &SqliteRepository,
    branch: &str,
    parent: Option<oak_core::Hash>,
    body: &[u8],
) -> Commit {
    let blob = repo.put_blob(body.to_vec()).unwrap();
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "proof.txt".into(),
        blob_hash: blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::with_timestamp(
        branch.into(),
        parent,
        None,
        manifest.hash,
        "qa".into(),
        None,
        vec![],
        chrono::Utc::now(),
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    commit
}

async fn fixture(
    headless_target: bool,
) -> (
    tempfile::TempDir,
    SqliteRepository,
    Commit,
    Commit,
    Commit,
    MockServer,
) {
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let main = commit(&repo, "main", None, b"main");
    let remote = commit(&repo, "feature", Some(main.hash.clone()), b"remote");
    let local = commit(
        &repo,
        "feature",
        Some(remote.hash.clone()),
        b"local-unpushed",
    );
    let mut feature = Branch::new(
        "feature".into(),
        Some("local description".into()),
        Some("main".into()),
    );
    feature.status = BranchStatus::Open;
    repo.store_branch(&feature).unwrap();
    repo.set_branch_head("feature", &local.hash).unwrap();
    repo.store_branch(&Branch::new(
        "main".into(),
        Some("local main description".into()),
        None,
    ))
    .unwrap();
    repo.set_branch_head("main", &main.hash).unwrap();
    repo.set_current_branch("feature").unwrap();
    let server = MockServer::start().await;
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        repo.set_metadata(key, &value).unwrap();
    }
    let target = if headless_target {
        serde_json::json!({"name":"main"})
    } else {
        serde_json::json!({"name":"main","head":main.hash.to_string()})
    };
    Mock::given(method("GET")).and(path("/api/oak/oak/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[
            {"name":"feature","head":remote.hash.to_string(),"parent_branch":"main","description":"remote description","status":"closed"}, target
        ]}))).mount(&server).await;
    (temp, repo, main, remote, local, server)
}

fn assert_local_metadata_unchanged(repo: &SqliteRepository, local: &Commit) {
    assert_eq!(
        repo.get_branch_head("feature").unwrap().as_ref(),
        Some(&local.hash)
    );
    let branch = repo.get_branch("feature").unwrap().unwrap();
    assert_eq!(branch.description.as_deref(), Some("local description"));
    assert_eq!(branch.status, BranchStatus::Open);
    assert_eq!(branch.parent_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn remote_review_and_diff_use_remote_heads_without_mutating_local_metadata() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (temp, repo, _main, remote, local, _server) = fixture(false).await;
    oak_cli::output::begin_capture();
    oak_cli::commands::review::remote_branch_review_json(
        temp.path(),
        "feature",
        true,
        "main",
        None,
        0,
    )
    .await
    .unwrap();
    let review: serde_json::Value =
        serde_json::from_str(oak_cli::output::end_capture().trim()).unwrap();
    assert_eq!(review["branch_head"], remote.hash.to_string());
    let caveats = review["merge_preview"]["caveats"].as_array().unwrap();
    assert!(caveats.iter().any(|value| value
        .as_str()
        .is_some_and(|text| text.contains("preceding remote fetch"))));
    assert!(!caveats.iter().any(|value| value
        .as_str()
        .is_some_and(|text| text.contains("No remote fetch was performed"))));
    assert_local_metadata_unchanged(&repo, &local);

    oak_cli::output::begin_capture();
    oak_cli::commands::review::remote_branch_diff_json(
        temp.path(),
        "feature",
        "main",
        oak_cli::commands::review::DiffMode::Tree,
        &[],
        Default::default(),
    )
    .await
    .unwrap();
    let diff: serde_json::Value =
        serde_json::from_str(oak_cli::output::end_capture().trim()).unwrap();
    assert_eq!(diff["branch_head"], remote.hash.to_string());
    assert_local_metadata_unchanged(&repo, &local);

    oak_cli::output::begin_capture();
    oak_cli::commands::review::remote_branch_diff_json(
        temp.path(),
        "feature",
        "main",
        oak_cli::commands::review::DiffMode::NetMerge,
        &[],
        oak_cli::commands::review::DiffJsonOptions {
            hunks: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let net: serde_json::Value =
        serde_json::from_str(oak_cli::output::end_capture().trim()).unwrap();
    assert_eq!(net["changed_file_count"], 1, "{net}");
    assert!(net["changed_files"][0]["patch"].as_str().is_some(), "{net}");
    assert_local_metadata_unchanged(&repo, &local);
}

#[tokio::test]
async fn remote_triage_preserves_local_metadata_and_uses_remote_head() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (temp, repo, _main, remote, local, _server) = fixture(false).await;
    oak_cli::output::begin_capture();
    oak_cli::commands::triage::remote_branch_triage_json(
        temp.path(),
        "main",
        None,
        oak_cli::commands::triage::AnalysisDepth::Summary,
        None,
        None,
    )
    .await
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(oak_cli::output::end_capture().trim()).unwrap();
    assert_eq!(json["rows"][0]["branch_head"], remote.hash.to_string());
    assert_local_metadata_unchanged(&repo, &local);
}

#[tokio::test]
async fn missing_remote_target_and_headless_target_never_fall_back_to_local_ref() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (temp, repo, _main, _remote, local, _server) = fixture(false).await;
    assert!(oak_cli::commands::review::remote_branch_review_json(
        temp.path(),
        "feature",
        true,
        "deleted-target",
        None,
        0,
    )
    .await
    .is_err());
    assert_local_metadata_unchanged(&repo, &local);
    assert!(oak_cli::commands::review::remote_branch_diff_json(
        temp.path(),
        "feature",
        "deleted-target",
        oak_cli::commands::review::DiffMode::Tree,
        &[],
        Default::default(),
    )
    .await
    .is_err());
    assert_local_metadata_unchanged(&repo, &local);

    let (temp, repo, _main, _remote, local, _server) = fixture(true).await;
    oak_cli::output::begin_capture();
    oak_cli::commands::review::remote_branch_review_json(
        temp.path(),
        "feature",
        true,
        "main",
        None,
        0,
    )
    .await
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(oak_cli::output::end_capture().trim()).unwrap();
    assert!(json.get("merge_preview").is_some());
    assert_eq!(
        json["merge_preview"]["prediction_available"], false,
        "{json}"
    );
    assert_local_metadata_unchanged(&repo, &local);
}

#[tokio::test]
async fn concurrent_local_head_advance_survives_remote_review_fetch() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let main = commit(&repo, "main", None, b"main");
    let before = commit(&repo, "feature", Some(main.hash.clone()), b"before");
    let during = commit(&repo, "feature", Some(main.hash.clone()), b"during");
    repo.store_branch(&Branch::new(
        "feature".into(),
        Some("local description".into()),
        Some("main".into()),
    ))
    .unwrap();
    repo.set_branch_head("feature", &before.hash).unwrap();
    repo.set_branch_head("main", &main.hash).unwrap();
    let remote_manifest = Manifest::new(vec![ManifestEntry {
        path: "proof.txt".into(),
        blob_hash: oak_core::hash_bytes(b"remote"),
        mode: FileMode::Regular,
    }]);
    let remote = Commit::with_timestamp(
        "feature".into(),
        Some(main.hash.clone()),
        None,
        remote_manifest.hash.clone(),
        "qa".into(),
        None,
        vec![],
        chrono::Utc::now(),
    )
    .unwrap();
    let server = MockServer::start().await;
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        repo.set_metadata(key, &value).unwrap();
    }
    Mock::given(method("GET")).and(path("/api/oak/oak/branches")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[
        {"name":"feature","head":remote.hash.to_string(),"parent_branch":"main"}, {"name":"main","head":main.hash.to_string()}
    ]}))).mount(&server).await;
    let trees = oak_core::build_tree(&remote_manifest.entries)
        .unwrap()
        .trees
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect::<Vec<_>>();
    let wire = oak_core::protocol::CommitData {
        hash: remote.hash.to_string(),
        branch_name: remote.branch_name.clone(),
        parent_hash: remote.parent_hash.as_ref().map(ToString::to_string),
        merge_parent_hash: None,
        manifest_hash: remote.manifest_hash.to_string(),
        author: remote.author.clone(),
        message: None,
        timestamp: remote.timestamp.to_rfc3339(),
        files: vec![],
    };
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(serde_json::json!({"commits":[wire],"trees":trees})),
        )
        .mount(&server)
        .await;
    let path = temp.path().to_path_buf();
    let review = tokio::spawn(async move {
        oak_cli::commands::review::remote_branch_review_json(
            &path, "feature", true, "main", None, 0,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    SqliteRepository::open(&temp.path().join(".oak/oak.db"))
        .unwrap()
        .set_branch_head("feature", &during.hash)
        .unwrap();
    review.await.unwrap().unwrap();
    assert_eq!(
        repo.get_branch_head("feature").unwrap().as_ref(),
        Some(&during.hash)
    );
}
