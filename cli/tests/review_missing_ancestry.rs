//! Controlled shallow metadata boundary with exact request and state oracles.
use oak_core::{
    Branch, Commit, FileMode, Hash, Manifest, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn commit(branch: &str, parent: Hash, manifest: &Manifest) -> Commit {
    Commit::with_timestamp(
        branch.into(),
        Some(parent),
        None,
        manifest.hash.clone(),
        "qa".into(),
        None,
        vec![],
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    )
    .unwrap()
}

async fn scenario(base_cached: bool, shape: &str) -> serde_json::Value {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    std::fs::write(
        temp.path().join("local-untracked"),
        b"preserve local bytes\n",
    )
    .unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let entry = |name: &str, bytes: &[u8]| ManifestEntry {
        path: name.into(),
        blob_hash: repo.put_blob(bytes.to_vec()).unwrap(),
        mode: FileMode::Regular,
    };
    let base_manifest = Manifest::new(vec![entry("base", b"base\n")]);
    let mut feature_entries = base_manifest.entries.clone();
    feature_entries.push(entry("feature", b"feature\n"));
    let feature_manifest = Manifest::new(feature_entries);
    let mut main_entries = base_manifest.entries.clone();
    main_entries.push(entry("main", b"main\n"));
    let main_manifest = Manifest::new(main_entries);
    for manifest in [&base_manifest, &feature_manifest, &main_manifest] {
        repo.store_manifest(manifest).unwrap();
    }
    // Q is absent deliberately. Once A is known it is a proven common base,
    // so its older parent Q must not become a prerequisite for this review.
    let older = oak_core::hash_bytes(b"unrelated older ancestor Q");
    let base = commit("main", older.clone(), &base_manifest);
    let left = commit("left", base.hash.clone(), &base_manifest);
    let right = commit("right", base.hash.clone(), &base_manifest);
    let mut feature = commit(
        "feature",
        if shape == "deeper" {
            left.hash.clone()
        } else {
            base.hash.clone()
        },
        &feature_manifest,
    );
    let main = commit(
        "main",
        if shape == "deeper" {
            right.hash.clone()
        } else {
            base.hash.clone()
        },
        &main_manifest,
    );
    let competitor = oak_core::hash_bytes(b"missing competing merge parent");
    if shape == "merge_parent" {
        feature = Commit::with_timestamp(
            "feature".into(),
            Some(base.hash.clone()),
            Some(competitor.clone()),
            feature_manifest.hash.clone(),
            "qa".into(),
            None,
            vec![],
            feature.timestamp,
        )
        .unwrap();
    }
    if shape == "cap" {
        let mut frontier: Vec<_> = (0..256)
            .map(|i| oak_core::hash_bytes(format!("missing-{i}").as_bytes()))
            .collect();
        while frontier.len() > 1 {
            frontier = frontier
                .chunks(2)
                .map(|pair| {
                    let node = Commit::with_timestamp(
                        "feature".into(),
                        Some(pair[0].clone()),
                        Some(pair[1].clone()),
                        feature_manifest.hash.clone(),
                        "qa".into(),
                        None,
                        vec![],
                        feature.timestamp,
                    )
                    .unwrap();
                    repo.store_commit(&node).unwrap();
                    node.hash
                })
                .collect();
        }
        feature = commit("feature", frontier.pop().unwrap(), &feature_manifest);
    }
    if base_cached {
        repo.store_commit(&base).unwrap();
    }
    for item in [&feature, &main] {
        repo.store_commit(item).unwrap();
    }
    for (name, head) in [("feature", &feature.hash), ("main", &main.hash)] {
        repo.store_branch(&Branch::new(
            name.into(),
            Some("local description".into()),
            (name == "feature").then(|| "main".into()),
        ))
        .unwrap();
        repo.set_branch_head(name, head).unwrap();
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
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[
            {"name":"feature","head":feature.hash.to_string(),"parent_branch":"main"},
            {"name":"main","head":if shape == "headless" { None } else { Some(main.hash.to_string()) }}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let wire = |item: &Commit| oak_core::protocol::CommitData {
        hash: item.hash.to_string(),
        branch_name: item.branch_name.clone(),
        parent_hash: item.parent_hash.as_ref().map(ToString::to_string),
        merge_parent_hash: item.merge_parent_hash.as_ref().map(ToString::to_string),
        manifest_hash: item.manifest_hash.to_string(),
        author: item.author.clone(),
        message: None,
        timestamp: item.timestamp.to_rfc3339(),
        files: vec![],
    };
    let trees: Vec<_> = oak_core::build_tree(&base_manifest.entries)
        .unwrap()
        .trees
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect();
    let base_response = if shape == "absent" {
        ResponseTemplate::new(404)
    } else {
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"commits":[wire(&base)],"trees":trees}))
    };
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(
            serde_json::json!({"hashes":[base.hash.to_string()]}),
        ))
        .respond_with(base_response)
        .mount(&server)
        .await;
    let mut intermediate = vec![left.hash.to_string(), right.hash.to_string()];
    intermediate.sort();
    if shape == "deeper" {
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/commits/info"))
            .and(body_json(serde_json::json!({"hashes":intermediate})))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"commits":[wire(&left),wire(&right)],"trees":trees}),
            ))
            .mount(&server)
            .await;
    }
    oak_cli::output::begin_capture();
    let result = if shape == "tree" {
        oak_cli::commands::review::remote_branch_diff_json(
            temp.path(),
            "feature",
            "main",
            oak_cli::commands::review::DiffMode::Tree,
            &[],
            Default::default(),
        )
        .await
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
    };
    let captured = oak_cli::output::end_capture();
    let json: serde_json::Value = match result {
        Ok(()) => serde_json::from_str(captured.trim()).unwrap(),
        Err(error) => {
            assert!(captured.trim().is_empty());
            serde_json::json!({"error":error.to_string()})
        }
    };
    let requests = server.received_requests().await.unwrap();
    let info: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/commits/info"))
        .collect();
    let expected: Vec<serde_json::Value> = match shape {
        "tree" | "headless" | "cap" => vec![],
        "deeper" => vec![
            serde_json::json!({"hashes":intermediate}),
            serde_json::json!({"hashes":[base.hash.to_string()]}),
        ],
        "merge_parent" => vec![serde_json::json!({"hashes":[competitor.to_string()]})],
        _ if base_cached => vec![],
        _ => vec![serde_json::json!({"hashes":[base.hash.to_string()]})],
    };
    assert_eq!(info.len(), expected.len());
    for (request, expected) in info.iter().zip(expected) {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            expected
        );
    }
    assert_eq!(requests.len(), 1 + info.len(), "no unrelated downloads");
    assert!(repo.get_commit(&older).unwrap().is_none());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("feature")
    );
    assert_eq!(repo.get_branch_head("feature").unwrap(), Some(feature.hash));
    assert_eq!(repo.get_branch_head("main").unwrap(), Some(main.hash));
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("local description")
    );
    assert_eq!(
        std::fs::read(temp.path().join("local-untracked")).unwrap(),
        b"preserve local bytes\n"
    );
    eprintln!(
        "OBSERVATION base_cached={base_cached} shape={shape} info_requests={} preview={} caveats={}",
        info.len(),
        json["merge_preview"],
        json["caveats"]
    );
    if !["tree", "headless", "cap", "absent", "merge_parent"].contains(&shape) {
        assert_eq!(
            json["merge_preview"]["merge_lineage_evidence"]["fork_point"],
            base.hash.to_string()
        );
        assert_eq!(json["merge_preview"]["merge_safety"]["certified"], true);
        assert_eq!(json["merge_preview"]["changed_files"][0]["path"], "feature");
    }
    json
}

#[tokio::test]
async fn remote_review_hydrates_only_missing_necessary_base_metadata() {
    let control = scenario(true, "simple").await;
    assert_eq!(
        control["merge_preview"]["prediction_available"], true,
        "cached-base control"
    );
    let shallow = scenario(false, "simple").await;
    assert_eq!(
        shallow["merge_preview"]["prediction_available"], true,
        "one available missing base should not require a whole-history fetch"
    );
}

#[tokio::test]
async fn missing_ancestry_requires_multiple_bounded_rounds() {
    let output = scenario(false, "deeper").await;
    assert_eq!(output["merge_preview"]["prediction_available"], true);
}

#[tokio::test]
async fn unavailable_ancestry_and_competing_merge_parent_never_certify() {
    for (cached, shape) in [(false, "absent"), (true, "merge_parent"), (true, "cap")] {
        let output = scenario(cached, shape).await;
        assert!(output["error"]
            .as_str()
            .unwrap()
            .contains("review preparation incomplete"));
        if shape == "cap" {
            assert!(output["error"]
                .as_str()
                .unwrap()
                .contains("cumulative exact-commit budget"));
        }
    }
}

#[tokio::test]
async fn tree_diff_and_headless_target_do_not_acquire_ancestry() {
    for shape in ["tree", "headless"] {
        let output = scenario(false, shape).await;
        assert!(output.get("error").is_none(), "{output}");
        if shape == "headless" {
            assert_ne!(output["merge_preview"]["prediction_available"], true);
        }
    }
}
