//! Controlled content-only gap through the real remote review entrypoint.
use oak_core::{
    Branch, Commit, FileMode, Manifest, ManifestEntry, MetadataKey, Repository, SqliteRepository,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn review(base_cached: bool, unrelated_cached: bool) -> serde_json::Value {
    review_response(base_cached, unrelated_cached, 200, None, false).await
}

async fn review_response(
    base_cached: bool,
    unrelated_cached: bool,
    status: u16,
    marker: Option<MetadataKey>,
    corrupt: bool,
) -> serde_json::Value {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    std::fs::write(temp.path().join("dirty"), b"preserve human work\n").unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let original = b"one\ntwo\nthree\nfour\nfive\n";
    let missing = oak_core::hash_bytes(original);
    if let Some(marker) = marker {
        repo.set_metadata(marker, missing.as_str()).unwrap();
    }
    let unrelated = oak_core::hash_bytes(b"unchanged content deliberately not hydrated");
    if unrelated_cached {
        repo.put_blob(b"unchanged content deliberately not hydrated".to_vec())
            .unwrap();
    }
    if base_cached {
        repo.put_blob(original.to_vec()).unwrap();
    }
    let mut commits = Vec::new();
    for (name, content) in [
        ("base", original.as_slice()),
        ("feature", b"ONE\ntwo\nthree\nfour\nfive\n".as_slice()),
        ("main", b"one\ntwo\nthree\nfour\nFIVE\n".as_slice()),
    ] {
        let blob_hash = if name == "base" {
            missing.clone()
        } else {
            repo.put_blob(content.to_vec()).unwrap()
        };
        let manifest = Manifest::new(vec![
            ManifestEntry {
                path: "file.txt".into(),
                blob_hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "unchanged.bin".into(),
                blob_hash: unrelated.clone(),
                mode: FileMode::Regular,
            },
        ]);
        repo.store_manifest(&manifest).unwrap();
        let parent = commits.first().map(|commit: &Commit| commit.hash.clone());
        let commit = Commit::with_timestamp(
            name.into(),
            parent,
            None,
            manifest.hash,
            "qa".into(),
            None,
            vec![],
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        commits.push(commit);
    }
    for (name, index) in [("feature", 1), ("main", 2)] {
        repo.store_branch(&Branch::new(
            name.into(),
            Some("local description".into()),
            (name == "feature").then(|| "main".into()),
        ))
        .unwrap();
        repo.set_branch_head(name, &commits[index].hash).unwrap();
    }
    repo.set_current_branch("feature").unwrap();
    if base_cached && corrupt {
        let target_blob = repo
            .get_manifest(&commits[2].manifest_hash)
            .unwrap()
            .unwrap()
            .get("file.txt")
            .unwrap()
            .blob_hash
            .clone();
        rusqlite::Connection::open(temp.path().join(".oak/oak.db"))
            .unwrap()
            .execute(
                "UPDATE blobs SET content=?1, size=?2, codec=0 WHERE hash=?3",
                rusqlite::params![b"SECRET_CORRUPT_CACHE".as_slice(), 20, target_blob.as_str()],
            )
            .unwrap();
    }
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
            {"name":"feature","head":commits[1].hash.to_string(),"parent_branch":"main"},
            {"name":"main","head":commits[2].hash.to_string()}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let raw_path = format!("/api/oak/oak/raw/{}/file.txt", commits[0].hash);
    Mock::given(method("GET"))
        .and(path(raw_path.clone()))
        .respond_with(
            ResponseTemplate::new(status).set_body_bytes(if corrupt || status != 200 {
                b"SECRET_UNTRUSTED_RESPONSE".as_slice()
            } else {
                original.as_slice()
            }),
        )
        .mount(&server)
        .await;
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::review::remote_branch_review_json(
        temp.path(),
        "feature",
        true,
        "main",
        None,
        0,
    )
    .await;
    let captured = oak_cli::output::end_capture();
    result.unwrap();
    let output: serde_json::Value = serde_json::from_str(captured.trim()).unwrap();
    assert!(!captured.contains("SECRET_UNTRUSTED_RESPONSE"));
    assert!(!captured.contains("SECRET_CORRUPT_CACHE"));
    assert_eq!(
        repo.get_blob(&missing).unwrap().is_some(),
        base_cached || (status == 200 && marker.is_none() && !corrupt)
    );
    let requests = server.received_requests().await.unwrap();
    let blobs: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == raw_path)
        .collect();
    assert!(blobs.len() <= 1);
    for request in &blobs {
        assert!(request.body.is_empty());
    }
    assert_eq!(
        requests.len(),
        1 + blobs.len(),
        "no commit metadata, ordinary pull, chunks, or unrelated content requests"
    );
    assert_eq!(
        repo.get_blob(&unrelated).unwrap().is_some(),
        unrelated_cached
    );
    assert_eq!(
        repo.get_branch_head("feature").unwrap(),
        Some(commits[1].hash.clone())
    );
    assert_eq!(
        repo.get_branch_head("main").unwrap(),
        Some(commits[2].hash.clone())
    );
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("feature")
    );
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("local description")
    );
    assert_eq!(
        std::fs::read(temp.path().join("dirty")).unwrap(),
        b"preserve human work\n"
    );
    eprintln!("CONTENT_OBSERVATION base_cached={base_cached} unrelated_cached={unrelated_cached} raw_requests={} fork={} prediction={} certified={} caveats={}",blobs.len(),output["merge_lineage_evidence"]["fork_point"],output["merge_preview"]["prediction_available"],output["merge_preview"]["merge_safety"]["certified"],output["merge_preview"]["caveats"]);
    assert_eq!(
        output["merge_lineage_evidence"]["fork_point"],
        commits[0].hash.to_string()
    );
    if base_cached || marker.is_some() {
        assert!(blobs.is_empty());
    } else {
        assert_eq!(blobs.len(), 1);
    }
    output
}

#[tokio::test]
async fn remote_review_fetches_only_required_cold_baseline_content() {
    let warm = review(true, true).await;
    assert_eq!(warm["merge_preview"]["prediction_available"], true);
    assert_eq!(warm["merge_preview"]["clean"], true);
    assert_eq!(warm["merge_preview"]["merge_safety"]["certified"], true);
    let cold = review(false, true).await;
    for control in [review(true, false).await, review(false, false).await] {
        assert_eq!(control["merge_preview"]["prediction_available"], true);
        assert_eq!(control["merge_preview"]["merge_safety"]["certified"], true);
        assert_eq!(
            control["merge_preview"]["changed_files"],
            warm["merge_preview"]["changed_files"]
        );
    }
    assert_eq!(
        cold["merge_preview"]["prediction_available"], true,
        "required baseline blob is available from the exact endpoint"
    );
    assert_eq!(cold["merge_preview"]["clean"], true);
    assert_eq!(cold["merge_preview"]["merge_safety"]["certified"], true);
    for status in [403, 404, 405, 500] {
        let unknown = review_response(false, false, status, None, false).await;
        assert_eq!(unknown["merge_preview"]["prediction_available"], false);
        assert_ne!(unknown["merge_preview"]["merge_safety"]["certified"], true);
        // A missing merge-base blob does not invalidate independently verified
        // M/F snapshot statistics; only the merge prediction is unavailable.
        assert_eq!(unknown["changed_files"], warm["changed_files"]);
    }
    let corrupt = review_response(false, false, 200, None, true).await;
    assert_eq!(corrupt["merge_preview"]["prediction_available"], false);
    let corrupt_cache = review_response(true, false, 200, None, true).await;
    assert_eq!(
        corrupt_cache["merge_preview"]["prediction_available"],
        false
    );
    assert_eq!(corrupt_cache["merge_allowed"], false);
    for file in corrupt_cache["changed_files"].as_array().unwrap() {
        assert_eq!(file["stats_available"], false);
        assert!(file.get("additions").is_none());
        assert!(file.get("deletions").is_none());
        assert_eq!(file["content_unavailable_reason"], "unverified_content");
    }
    for marker in [MetadataKey::RestrictedBlobs, MetadataKey::KnownLostBlobs] {
        let unknown = review_response(false, false, 200, Some(marker), false).await;
        assert_eq!(unknown["merge_preview"]["prediction_available"], false);
        assert_ne!(unknown["merge_preview"]["merge_safety"]["certified"], true);
    }
}

#[tokio::test]
async fn remote_review_reconstructs_required_empty_blob_without_raw_support() {
    review_empty_blob("main").await;
}

#[tokio::test]
async fn snapshot_only_review_reconstructs_empty_content_without_certifying_other_target() {
    review_empty_blob("another-target").await;
}

async fn review_empty_blob(parent: &str) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let empty = oak_core::Blob::empty_hash();
    let mut commits = Vec::new();
    for (name, mode) in [
        ("main", FileMode::Regular),
        ("feature", FileMode::Executable),
    ] {
        let manifest = Manifest::new(vec![ManifestEntry {
            path: "empty-script".into(),
            blob_hash: empty.clone(),
            mode,
        }]);
        repo.store_manifest(&manifest).unwrap();
        let commit = Commit::with_timestamp(
            name.into(),
            commits.first().map(|commit: &Commit| commit.hash.clone()),
            None,
            manifest.hash,
            "qa".into(),
            None,
            vec![],
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
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
        commits.push(commit);
    }
    repo.set_current_branch("feature").unwrap();
    assert!(!repo.has_blob(&empty).unwrap());
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
                {"name":"feature","head":commits[1].hash.to_string(),"parent_branch":parent},
                {"name":"main","head":commits[0].hash.to_string()}
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // All raw routes deliberately return the mock server's default 404.
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::review::remote_branch_review_json(
        temp.path(),
        "feature",
        true,
        "main",
        None,
        0,
    )
    .await;
    let output = oak_cli::output::end_capture();
    result.unwrap();
    let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(
        json["merge_preview"]["prediction_available"],
        parent == "main",
        "{json}"
    );
    if parent == "main" {
        assert_eq!(json["merge_preview"]["merge_safety"]["certified"], true);
    } else {
        assert_ne!(json["merge_preview"]["merge_safety"]["certified"], true);
    }
    assert_eq!(json["changed_file_count"], 1);
    assert_ne!(json["changed_files"][0]["stats_available"], false);
    assert!(json["changed_files"][0]
        .get("content_unavailable_reason")
        .is_none());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "empty content is known without raw GET"
    );
    assert_eq!(
        repo.get_blob(&empty).unwrap().unwrap().content,
        Vec::<u8>::new()
    );
    assert_eq!(
        repo.get_branch_head("feature").unwrap(),
        Some(commits[1].hash.clone())
    );
}
