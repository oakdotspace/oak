use oak_core::{
    Branch, Commit, FileMode, Manifest, ManifestEntry, MetadataKey, Repository, SqliteRepository,
};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn remote_review_hydrates_verified_base_missing_from_metadata_only_ancestry() {
    run(false, false, false).await;
}

#[tokio::test]
async fn remote_triage_hydrates_verified_base_missing_from_metadata_only_ancestry() {
    run(true, false, false).await;
}

#[tokio::test]
async fn remote_review_repairs_missing_nested_base_tree() {
    run(false, true, false).await;
}

#[tokio::test]
async fn base_tree_hydration_does_not_certify_missing_blob_content() {
    run(false, false, true).await;
}

async fn run(triage: bool, partial_tree: bool, missing_blob: bool) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let hash = if missing_blob {
        oak_core::hash_bytes(b"base")
    } else {
        repo.put_blob(b"base".to_vec()).unwrap()
    };
    let base_manifest = Manifest::new(vec![ManifestEntry {
        path: "dir/file".into(),
        blob_hash: hash,
        mode: FileMode::Regular,
    }]);
    let base = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        base_manifest.hash.clone(),
        "qa".into(),
        None,
        vec![],
        chrono::Utc::now(),
    )
    .unwrap();
    repo.store_commit(&base).unwrap();
    if partial_tree {
        let built = oak_core::build_tree(&base_manifest.entries).unwrap();
        repo.store_tree(
            built
                .trees
                .iter()
                .find(|tree| tree.hash == base_manifest.hash)
                .unwrap(),
        )
        .unwrap();
    }
    let next_hash = repo.put_blob(b"next".to_vec()).unwrap();
    let next = Manifest::new(vec![ManifestEntry {
        path: "file".into(),
        blob_hash: next_hash,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&next).unwrap();
    let mut heads = Vec::new();
    for name in ["feature", "main"] {
        let commit = Commit::with_timestamp(
            name.into(),
            Some(base.hash.clone()),
            None,
            next.hash.clone(),
            "qa".into(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        repo.store_branch(&Branch::new(
            name.into(),
            None,
            (name == "feature").then(|| "main".into()),
        ))
        .unwrap();
        repo.set_branch_head(name, &commit.hash).unwrap();
        heads.push(commit.hash.to_string());
    }
    repo.set_current_branch("feature").unwrap();
    let server = MockServer::start().await;
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        repo.set_metadata(key, &value).unwrap();
    }
    Mock::given(method("GET")).and(path("/api/oak/oak/branches")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[{"name":"feature","head":heads[0],"parent_branch":"main"},{"name":"main","head":heads[1]},{"name":"feature-copy","head":heads[0],"parent_branch":"main"}]}))).mount(&server).await;
    let trees: Vec<_> = oak_core::build_tree(&base_manifest.entries)
        .unwrap()
        .trees
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect();
    let wire = oak_core::protocol::CommitData {
        hash: base.hash.to_string(),
        branch_name: base.branch_name.clone(),
        parent_hash: None,
        merge_parent_hash: None,
        manifest_hash: base.manifest_hash.to_string(),
        author: base.author.clone(),
        message: None,
        timestamp: base.timestamp.to_rfc3339(),
        files: vec![],
    };
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(
            serde_json::json!({"hashes":[base.hash.to_string()]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"commits":[wire],"trees":trees})),
        )
        .expect(1)
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    if triage {
        oak_cli::commands::triage::remote_branch_triage_json(
            temp.path(),
            "main",
            None,
            oak_cli::commands::triage::AnalysisDepth::Full,
            None,
            None,
        )
        .await
        .unwrap();
    } else {
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
    }
    let captured = oak_cli::output::end_capture();
    let json: serde_json::Value = serde_json::from_str(captured.trim()).unwrap();
    if !triage {
        assert_eq!(
            json["merge_preview"]["prediction_available"], !missing_blob,
            "{json}"
        );
        if missing_blob {
            assert!(captured.contains("blob data is incomplete"), "{captured}");
        }
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
        oak_cli::output::end_capture();
    } else {
        assert_eq!(json["branches_analyzed"], 2);
    }
    assert!(
        repo.get_manifest(&base_manifest.hash).unwrap().is_some(),
        "base manifest should be hydrated, not merely its commit metadata"
    );
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("feature")
    );
}
