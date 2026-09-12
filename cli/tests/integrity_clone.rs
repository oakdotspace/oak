use oak_cli::commands::repo;
use oak_core::{
    hash_bytes, Commit, FileMode, MetadataKey, Repository, SqliteRepository, Tree, TreeEntry,
    TreeEntryKind,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_capability(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "credential_presented": true,
            "credential_accepted": true
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn clone_exact_branch_head_is_bound_before_destination_creation() {
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("proof_profile", "bounded_v1"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("expected_head", expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "pinned-snapshot",
            "selected_branch": {"name": "feature", "head": expected, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", "pinned-snapshot"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("expected_head", expected))
        .respond_with(ResponseTemplate::new(503).set_body_string("pinned pull sentinel"))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            expected,
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pinned pull sentinel"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn pinned_full_clone_selects_exact_branch_without_narrowing_history() {
    let tree = Tree::new(Vec::new()).unwrap();
    let main = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let feature = Commit::with_timestamp(
        "feature".to_string(),
        Some(main.hash.clone()),
        None,
        tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let newer_feature = Commit::with_timestamp(
        "feature".to_string(),
        Some(feature.hash.clone()),
        None,
        tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let branch_wire = |name: &str, parent: Option<&str>| {
        serde_json::json!({
            "name": name,
            "description": null,
            "parent_branch": parent,
            "status": "open",
            "created_at": "2026-01-01T00:00:00Z"
        })
    };
    let commit_wire = |commit: &Commit| {
        serde_json::json!({
            "hash": commit.hash,
            "branch_name": commit.branch_name,
            "parent_hash": commit.parent_hash,
            "merge_parent_hash": commit.merge_parent_hash,
            "manifest_hash": commit.manifest_hash,
            "author": commit.author,
            "message": commit.message,
            "timestamp": commit.timestamp.to_rfc3339(),
            "files": []
        })
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("expected_head", feature.hash.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "pinned-snapshot",
            "selected_branch": {"name": "feature", "head": feature.hash, "status": "open"},
            "scope": {"commit_count": 3, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("expected_head", feature.hash.as_str()))
        .and(query_param("integrity_snapshot", "pinned-snapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": main.hash,
            "branch": branch_wire("main", None),
            "branches": [branch_wire("main", None), branch_wire("feature", Some("main"))],
            "commits": [commit_wire(&main), commit_wire(&feature), commit_wire(&newer_feature)],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            feature.hash.as_str(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cloned = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    assert_eq!(
        cloned.get_current_branch_name().unwrap().as_deref(),
        Some("feature")
    );
    assert_eq!(cloned.get_head().unwrap().as_ref(), Some(&feature.hash));
    assert!(cloned.get_commit(&main.hash).unwrap().is_some());
    assert!(cloned.get_commit(&feature.hash).unwrap().is_some());
    assert!(cloned.get_commit(&newer_feature.hash).unwrap().is_some());
    assert_eq!(
        cloned.get_branch_head("feature").unwrap().as_ref(),
        Some(&feature.hash),
        "a newer imported commit must not overwrite the selected identity"
    );
    assert!(!destination.join(".oak/CLONE_IN_PROGRESS").exists());

    let requests = server.received_requests().await.unwrap();
    for request in requests.iter().filter(|request| {
        matches!(
            request.url.path(),
            "/api/oak/repo/integrity" | "/api/oak/repo/pull"
        )
    }) {
        let query: std::collections::HashMap<_, _> =
            request.url.query_pairs().into_owned().collect();
        assert!(!query.contains_key("depth"), "unexpected narrowed history");
        assert!(
            !query.contains_key("branch") && !query.contains_key("branch_name"),
            "selected branch must not become full-history scope: {query:?}"
        );
    }
}

#[tokio::test]
async fn pinned_clone_never_uses_legacy_scope_override() {
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "repo"})))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            expected,
            "--allow-legacy-scope",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("never falls back"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn pinned_clone_drift_is_not_retried_and_leaves_no_destination() {
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "pinned-snapshot",
            "selected_branch": {"name": "feature", "head": expected, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            expected,
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("changed after exact-head preflight"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn incomplete_history_stops_before_destination_creation_or_pull() {
    let server = MockServer::start().await;
    mount_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "test-snapshot",
            "scope": {"commit_count": 2, "manifest_count": 2, "blob_count": 1, "chunk_count": 0},
            "findings": [{
                "code": "missing_blob_mapping",
                "blob_hash": "deadbeef",
                "affected": [{"commit": "old", "path": "legacy.bin"}],
                "recoverability": "requires_authoritative_bytes",
                "detail": "reachable blob deadbeef has no chunk mapping"
            }],
            "head_affected": false,
            "shallow_recovery_available": true,
            "recommended_next_commands": ["oak clone oak/repo --shallow"]
        })))
        .expect(1)
        .mount(&server)
        .await;

    // No /pull mock is mounted. Reaching the bulk-download phase would fail
    // with a different request and make the mock server expose the regression.
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let error = repo::clone_repo(&server.uri(), "oak/repo", &destination, false)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("content_incomplete"));
    assert!(error.to_string().contains("--shallow"));
    assert!(!destination.exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn clone_rejects_main_branch_before_local_or_remote_side_effects() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");

    let error = repo::clone_repo_sparse_on_branch(
        &server.uri(),
        "oak/repo",
        &destination,
        true,
        None,
        Some("main"),
    )
    .await
    .expect_err("main is server-only and must fail preflight");

    assert!(error
        .to_string()
        .contains("`main` only exists on the server"));
    assert!(!destination.exists());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn selected_branch_and_cone_are_proved_before_shallow_pull() {
    let server = MockServer::start().await;
    mount_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("depth", "1"))
        .and(query_param("branch", "feature"))
        .and(query_param("paths", "docs/api,src"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "test-snapshot",
            "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 0},
            "findings": [{
                "code": "missing_blob_mapping",
                "blob_hash": "deadbeef",
                "affected": [{"commit": "feature-head", "path": "src/main.rs"}],
                "recoverability": "requires_authoritative_bytes",
                "detail": "selected content is unavailable"
            }],
            "head_affected": true,
            "shallow_recovery_available": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let cone = oak_core::SparseCone::new(["src", "docs/api"]);
    let error = repo::clone_repo_sparse_on_branch(
        &server.uri(),
        "oak/repo",
        &destination,
        true,
        cone,
        Some("feature"),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("content_incomplete"));
    assert!(!destination.exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn supported_unpinned_full_clone_selects_branch_without_narrowing_history() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "sparse_materialization_protocol": "cone_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("paths", "docs,src"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("sparse_materialization_protocol", "cone_v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "sparse_materialization_protocol": "cone_v1",
            "snapshot_token": "full-snapshot",
            "selected_branch": {"name": "feature", "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "status": "open"},
            "scope": {"commit_count": 2, "manifest_count": 2, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("paths", "docs,src"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("sparse_materialization_protocol", "cone_v1"))
        .and(query_param("integrity_snapshot", "full-snapshot"))
        .respond_with(ResponseTemplate::new(503).set_body_string("pull sentinel"))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let cone = oak_core::SparseCone::new(["src", "docs"]);
    let error = repo::clone_repo_sparse_on_branch(
        &server.uri(),
        "oak/repo",
        &destination,
        false,
        cone,
        Some("feature"),
    )
    .await
    .expect_err("sentinel pull intentionally fails after preflight");
    assert!(error.to_string().contains("pull sentinel"), "{error}");

    let requests = server.received_requests().await.unwrap();
    let preflight = requests
        .iter()
        .find(|request| request.url.path() == "/api/oak/repo/integrity")
        .unwrap();
    let preflight_query: std::collections::HashMap<_, _> =
        preflight.url.query_pairs().into_owned().collect();
    assert!(!preflight_query.contains_key("branch"));
    assert!(!preflight_query.contains_key("depth"));
    assert_eq!(
        preflight_query.get("selected_branch").map(String::as_str),
        Some("feature")
    );
    assert!(!preflight_query.contains_key("expected_head"));
    assert_eq!(
        preflight_query.get("paths").map(String::as_str),
        Some("docs,src")
    );
    assert_eq!(
        preflight_query
            .get("sparse_materialization_protocol")
            .map(String::as_str),
        Some("cone_v1")
    );

    let pull = requests
        .iter()
        .find(|request| request.url.path() == "/api/oak/repo/pull")
        .unwrap();
    let pull_query: std::collections::HashMap<_, _> = pull.url.query_pairs().into_owned().collect();
    assert!(!pull_query.contains_key("branch_name"));
    assert!(!pull_query.contains_key("depth"));
    assert_eq!(
        pull_query.get("selected_branch").map(String::as_str),
        Some("feature")
    );
    assert!(!pull_query.contains_key("expected_head"));
    assert_eq!(
        pull_query.get("paths").map(String::as_str),
        Some("docs,src")
    );
    assert_eq!(
        pull_query
            .get("sparse_materialization_protocol")
            .map(String::as_str),
        Some("cone_v1")
    );
}

#[tokio::test]
async fn full_clone_preserves_adjudicated_historical_loss_with_healthy_head() {
    let server = MockServer::start().await;
    mount_capability(&server).await;
    let lost_hash = hash_bytes(b"unavailable historical bytes");
    let healthy_bytes = b"healthy current bytes".to_vec();
    let healthy_hash = hash_bytes(&healthy_bytes);
    let historical_tree = Tree::new(vec![TreeEntry {
        name: "Cargo.lock".to_string(),
        kind: TreeEntryKind::Blob,
        hash: lost_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let current_tree = Tree::new(vec![TreeEntry {
        name: "Cargo.lock".to_string(),
        kind: TreeEntryKind::Blob,
        hash: healthy_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let historical = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        historical_tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::Utc::now() - chrono::Duration::seconds(1),
    )
    .unwrap();
    let current = Commit::with_timestamp(
        "main".to_string(),
        Some(historical.hash.clone()),
        None,
        current_tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::Utc::now(),
    )
    .unwrap();
    let commit_wire = |commit: &Commit| {
        serde_json::json!({
            "hash": commit.hash,
            "branch_name": commit.branch_name,
            "parent_hash": commit.parent_hash,
            "merge_parent_hash": commit.merge_parent_hash,
            "manifest_hash": commit.manifest_hash,
            "author": commit.author,
            "message": commit.message,
            "timestamp": commit.timestamp.to_rfc3339(),
            "files": []
        })
    };

    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("known_loss_protocol", "report_v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "known_loss_protocol": "report_v1",
            "snapshot_token": "loss-snapshot",
            "scope": {"commit_count": 2, "manifest_count": 2, "blob_count": 2, "chunk_count": 1},
            "findings": [{
                "code": "known_lost_blob",
                "blob_hash": lost_hash,
                "recoverability": "operator_adjudicated_loss",
                "detail": "legacy bytes unavailable"
            }],
            "head_affected": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", "loss-snapshot"))
        .and(query_param("known_loss_protocol", "report_v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": current.hash,
            "branch": null,
            "branches": [{
                "name": "main",
                "description": null,
                "parent_branch": null,
                "status": "open",
                "created_at": "2026-01-01T00:00:00Z"
            }],
            "commits": [commit_wire(&historical), commit_wire(&current)],
            "blobs": [{
                "hash": healthy_hash,
                "content": [],
                "size": healthy_bytes.len(),
                "chunks": [{
                    "hash": healthy_hash,
                    "offset": 0,
                    "size": healthy_bytes.len()
                }]
            }],
            "trees": [
                oak_core::protocol::tree_to_wire(&historical_tree),
                oak_core::protocol::tree_to_wire(&current_tree)
            ],
            "missing_content": [{
                "kind": "blob",
                "hash": lost_hash,
                "reason_code": "operator_adjudicated_loss"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{
                "hash": healthy_hash,
                "download_url": null,
                "content": healthy_bytes
            }],
            "batch_url": null,
            "restricted": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    repo::clone_repo(&server.uri(), "oak/repo", &destination, false)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(destination.join("Cargo.lock")).unwrap(),
        healthy_bytes
    );
    let cloned = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    assert!(cloned.get_commit(&historical.hash).unwrap().is_some());
    assert!(cloned.get_tree(&historical_tree.hash).unwrap().is_some());
    assert!(cloned.get_blob(&lost_hash).unwrap().is_none());
    assert!(cloned
        .get_metadata(MetadataKey::KnownLostBlobs)
        .unwrap()
        .is_some_and(|value| value.lines().any(|hash| hash == lost_hash.as_str())));
}

#[tokio::test]
async fn complete_depth_one_squash_proof_allows_clone_to_reach_pull() {
    let server = MockServer::start().await;
    mount_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("depth", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "test-snapshot",
            "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "omitted_parent_edges": ["primary-parent", "merge-parent"]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("depth", "1"))
        .respond_with(ResponseTemplate::new(503).set_body_string("pull sentinel"))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let error = repo::clone_repo(&server.uri(), "oak/repo", &destination, true)
        .await
        .expect_err("sentinel pull intentionally fails after preflight");
    assert!(!error.to_string().contains("content_incomplete"), "{error}");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn api_key_environment_selects_and_sends_the_authenticated_clone_proof() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .and(header("authorization", "Bearer process-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "credential_presented": true,
            "credential_accepted": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(header("authorization", "Bearer process-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "test-snapshot",
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
            "findings": [{
                "code": "missing_chunk_object",
                "affected": [],
                "recoverability": "requires_authoritative_bytes",
                "detail": "authenticated proof sentinel"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("OAK_API_KEY", "process-api-key")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("authenticated proof sentinel"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn blank_api_key_environment_falls_back_to_the_stored_remote_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .and(header("authorization", "Bearer stored-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "credential_presented": true,
            "credential_accepted": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(header("authorization", "Bearer stored-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "test-snapshot",
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", "test-snapshot"))
        .and(header("authorization", "Bearer stored-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let oak_dir = home.path().join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    std::fs::write(
        oak_dir.join("credentials"),
        serde_json::to_vec(&serde_json::json!([{
            "server": server.uri(),
            "token": "stored-api-key",
            "username": "stored-user"
        }]))
        .unwrap(),
    )
    .unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_API_KEY", " \t ")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cloned = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    assert_eq!(
        cloned.get_metadata(MetadataKey::ApiKey).unwrap().as_deref(),
        Some("stored-api-key")
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn clone_does_not_persist_a_credential_the_server_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .and(header("authorization", "Bearer rejected-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "credential_presented": true,
            "credential_accepted": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(header("authorization", "Bearer rejected-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "public-snapshot",
            "scope": {"commit_count": 0, "manifest_count": 0, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", "public-snapshot"))
        .and(header("authorization", "Bearer rejected-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_API_KEY", "rejected-api-key")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cloned = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    assert_eq!(cloned.get_metadata(MetadataKey::ApiKey).unwrap(), None);
    assert!(String::from_utf8_lossy(&output.stderr).contains("not persisted"));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn clone_repreflights_once_on_snapshot_race_and_binds_nondefault_branch() {
    let tree = Tree::new(Vec::new()).unwrap();
    let feature = Commit::with_timestamp(
        "feature".to_string(),
        None,
        None,
        tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let feature_hash = feature.hash.to_string();
    let feature_hash_for_proof = feature_hash.clone();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(2)
        .mount(&server)
        .await;

    let proof_attempt = Arc::new(AtomicUsize::new(0));
    let proof_attempt_for_response = proof_attempt.clone();
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("verify", "metadata"))
        .and(query_param("depth", "1"))
        .and(query_param("branch", "feature"))
        .and(query_param("selected_branch", "feature"))
        .respond_with(move |_request: &wiremock::Request| {
            let attempt = proof_attempt_for_response.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": if attempt == 0 { "snapshot-a" } else { "snapshot-b" },
                "selected_branch": {"name": "feature", "head": feature_hash_for_proof, "status": "open"},
                "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": []
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("depth", "1"))
        .and(query_param("branch_name", "feature"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("integrity_snapshot", "snapshot-a"))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("depth", "1"))
        .and(query_param("branch_name", "feature"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("integrity_snapshot", "snapshot-b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": feature_hash,
            "branch": {
                "name": "feature",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": "2026-01-01T00:00:00Z"
            },
            "branches": [{
                "name": "feature",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": "2026-01-01T00:00:00Z"
            }],
            "commits": [{
                "hash": feature_hash,
                "branch_name": "feature",
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": tree.hash,
                "author": "tester",
                "message": null,
                "timestamp": feature.timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--shallow",
            "--branch",
            "feature",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(proof_attempt.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn supported_unpinned_clone_stops_after_second_selected_branch_race() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .expect(2)
        .mount(&server)
        .await;
    let proof_attempt = Arc::new(AtomicUsize::new(0));
    let proof_attempt_for_response = proof_attempt.clone();
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("selected_branch", "feature"))
        .respond_with(move |_request: &wiremock::Request| {
            let attempt = proof_attempt_for_response.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": if attempt == 0 { "selected-a" } else { "selected-b" },
                "selected_branch": {"name": "feature", "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "status": "open"},
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": []
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("selected_branch", "feature"))
        .respond_with(ResponseTemplate::new(412))
        .expect(2)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("changed again"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(proof_attempt.load(Ordering::SeqCst), 2);
    assert!(!destination.exists());
}

#[tokio::test]
async fn malformed_pinned_pull_removes_new_destination() {
    let server = MockServer::builder().start().await;
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "selected",
            "selected_branch": {"name": "feature", "head": expected, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            expected,
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to parse"));
    assert!(!destination.exists());
}

#[tokio::test]
async fn nonempty_destination_is_refused_without_touching_user_files() {
    let server = MockServer::builder().start().await;
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "selected",
            "selected_branch": {"name": "feature", "head": expected, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    std::fs::create_dir(&destination).unwrap();
    let sentinel = destination.join("selected.txt");
    std::fs::write(&sentinel, b"user bytes\n").unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--expected-head",
            expected,
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not empty"));
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"user bytes\n");
    assert!(!destination.join(".oak").exists());
}

#[tokio::test]
async fn selected_head_known_loss_is_rejected_even_when_default_head_is_healthy() {
    let server = MockServer::builder().start().await;
    let lost_hash = hash_bytes(b"selected bytes unavailable");
    let selected_tree = Tree::new(vec![TreeEntry {
        name: "selected.txt".to_string(),
        kind: TreeEntryKind::Blob,
        hash: lost_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let empty_tree = Tree::new(Vec::new()).unwrap();
    let main = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        empty_tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::Utc::now(),
    )
    .unwrap();
    let feature = Commit::with_timestamp(
        "feature".to_string(),
        Some(main.hash.clone()),
        None,
        selected_tree.hash.clone(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::Utc::now() + chrono::Duration::seconds(1),
    )
    .unwrap();
    let commit_wire = |commit: &Commit| {
        serde_json::json!({
            "hash": commit.hash,
            "branch_name": commit.branch_name,
            "parent_hash": commit.parent_hash,
            "merge_parent_hash": commit.merge_parent_hash,
            "manifest_hash": commit.manifest_hash,
            "author": commit.author,
            "message": commit.message,
            "timestamp": commit.timestamp.to_rfc3339(),
            "files": []
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("selected_branch", "feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "known_loss_protocol": "report_v1",
            "snapshot_token": "selected-loss",
            "selected_branch": {"name": "feature", "head": feature.hash, "status": "open"},
            "scope": {"commit_count": 2, "manifest_count": 2, "blob_count": 1, "chunk_count": 0},
            "findings": [{
                "code": "known_lost_blob",
                "blob_hash": lost_hash,
                "recoverability": "operator_adjudicated_loss",
                "detail": "legacy bytes unavailable"
            }],
            "head_affected": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("selected_branch", "feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": main.hash,
            "branch": {"name": "main", "description": null, "parent_branch": null, "status": "open", "created_at": "2026-01-01T00:00:00Z"},
            "branches": [
                {"name": "main", "description": null, "parent_branch": null, "status": "open", "created_at": "2026-01-01T00:00:00Z"},
                {"name": "feature", "description": null, "parent_branch": "main", "status": "open", "created_at": "2026-01-01T00:00:00Z"}
            ],
            "commits": [commit_wire(&main), commit_wire(&feature)],
            "blobs": [],
            "trees": [oak_core::protocol::tree_to_wire(&selected_tree)],
            "missing_content": [{"kind": "blob", "hash": lost_hash, "reason_code": "operator_adjudicated_loss"}]
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let error = repo::clone_repo_sparse_on_branch(
        &server.uri(),
        "oak/repo",
        &destination,
        false,
        None,
        Some("feature"),
    )
    .await
    .expect_err("selected-head loss must override a healthy default head");
    assert!(error.to_string().contains("reachable from selected head"));
    assert!(!destination.exists());
}
