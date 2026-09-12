use oak_core::protocol::tree_to_wire;
use oak_core::{
    hash_bytes, Commit, FileMode, Repository, SqliteRepository, Tree, TreeEntry, TreeEntryKind,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn commit_wire(commit: &Commit) -> serde_json::Value {
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
}

fn assert_valid_stat_cache(destination: &std::path::Path, expected: &[(&str, &oak_core::Hash)]) {
    let repo = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    let cache = repo.load_stat_cache().unwrap();
    assert_eq!(
        cache.len(),
        expected.len(),
        "unexpected stat-cache coverage"
    );
    for (path, blob_hash) in expected {
        let entry = cache
            .get(*path)
            .unwrap_or_else(|| panic!("missing cache row for {path}"));
        let metadata = std::fs::metadata(destination.join(path)).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        #[cfg(unix)]
        let ctime_ns = {
            use std::os::unix::fs::MetadataExt;
            metadata
                .ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(metadata.ctime_nsec())
        };
        #[cfg(not(unix))]
        let ctime_ns = 0;
        assert_eq!(entry.blob_hash, **blob_hash, "wrong cached blob for {path}");
        assert_eq!(
            entry.size,
            metadata.len() as i64,
            "wrong cached size for {path}"
        );
        assert_eq!(entry.mtime_ns, mtime_ns, "stale cached mtime for {path}");
        assert_eq!(entry.ctime_ns, ctime_ns, "stale cached ctime for {path}");
    }
}

#[tokio::test]
async fn clone_json_reports_exact_selected_identity_and_observed_empty_counts() {
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
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": true,
            "credential_accepted": true
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
            "snapshot_token": "snapshot",
            "selected_branch": {"name": "feature", "head": feature.hash, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let pull = serde_json::json!({
        "head": feature.hash,
        "branch": null,
        "branches": [{
            "name": "feature",
            "description": null,
            "parent_branch": null,
            "status": "open",
            "created_at": "2026-01-01T00:00:00Z"
        }],
        "commits": [commit_wire(&feature)],
        "blobs": [],
        "trees": [],
        "renames": [],
        "restricted_blobs": [],
        "missing_content": []
    });
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("integrity_snapshot", "snapshot"))
        .and(query_param("selected_branch", "feature"))
        .and(query_param("expected_head", feature.hash.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(&pull))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--allow-unverified-integrity",
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
        .env("OAK_API_KEY", "receipt-token")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak process");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid receipt {error}: {:?}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert!(!String::from_utf8_lossy(&output.stdout).contains("receipt-token"));
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["operation"], "clone");
    assert_eq!(receipt["repository"]["owner"], "oak");
    assert_eq!(receipt["repository"]["name"], "repo");
    assert_eq!(receipt["request"]["history"]["mode"], "full");
    assert_eq!(receipt["request"]["selected_branch"], "feature");
    assert_eq!(receipt["request"]["expected_head"], feature.hash.as_str());
    assert_eq!(receipt["result"]["local_branch"], "feature");
    assert_eq!(receipt["result"]["head"], feature.hash.as_str());
    assert_eq!(receipt["result"]["manifest"], tree.hash.as_str());
    assert_eq!(receipt["result"]["acquisition_profile"], "bounded_v1");
    assert_eq!(receipt["result"]["snapshot_bound"], true);
    assert_eq!(receipt["result"]["initial_pull_snapshot_bound"], true);
    assert_eq!(receipt["result"]["selected_branch_snapshot_bound"], true);
    assert_eq!(
        receipt["result"]["preflight_credential_disposition"],
        "server_reported_accepted"
    );
    assert_eq!(receipt["observed"]["object_entries_received"]["commits"], 1);
    assert_eq!(receipt["observed"]["object_entries_received"]["trees"], 0);
    assert_eq!(
        receipt["observed"]["object_entries_received"]["blob_descriptors"],
        0
    );
    assert_eq!(receipt["observed"]["chunks"]["required_unique"], 0);
    assert_eq!(receipt["observed"]["chunks"]["downloaded_unique"], 0);
    assert_eq!(receipt["observed"]["materialization"]["written_paths"], 0);
    assert_eq!(
        receipt["observed"]["materialization"]["missing_required_paths"],
        0
    );
    assert!(receipt["observed"]["elapsed_ms"].as_f64().is_some());
    assert_eq!(receipt["unknown"].as_array().unwrap().len(), 3);
    assert!(output.stderr.is_empty(), "structured stderr was not quiet");
}

#[tokio::test]
async fn clone_json_counts_verified_download_and_materialization_without_rescanning() {
    let content = b"receipt bytes".to_vec();
    let blob_hash = hash_bytes(&content);
    let tree = Tree::new(vec![TreeEntry {
        name: "file.txt".to_string(),
        kind: TreeEntryKind::Blob,
        hash: blob_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
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
            "snapshot_token": "snapshot",
            "selected_branch": {"name": "feature", "head": feature.hash, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
            "findings": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": feature.hash,
            "branch": null,
            "branches": [{
                "name": "feature",
                "description": null,
                "parent_branch": null,
                "status": "open",
                "created_at": "2026-01-01T00:00:00Z"
            }],
            "commits": [commit_wire(&feature)],
            "blobs": [{
                "hash": blob_hash,
                "content": [],
                "size": content.len(),
                "chunks": [{"hash": blob_hash, "offset": 0, "size": content.len()}]
            }],
            "trees": [tree_to_wire(&tree)],
            "renames": [],
            "restricted_blobs": [],
            "missing_content": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{"hash": blob_hash, "content": content}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
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

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.stdout.is_empty(),
        "receipt missing; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid receipt {error}: {:?}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(receipt["observed"]["chunks"]["required_unique"], 1);
    assert_eq!(receipt["observed"]["chunks"]["downloaded_unique"], 1);
    assert_eq!(
        receipt["observed"]["chunks"]["downloaded_logical_bytes"],
        content.len()
    );
    assert_eq!(receipt["observed"]["materialization"]["written_paths"], 1);
    assert_eq!(
        receipt["observed"]["materialization"]["logical_bytes"],
        content.len()
    );
    assert_eq!(
        std::fs::read(destination.join("file.txt")).unwrap(),
        content
    );
}

#[tokio::test]
async fn legacy_selected_clone_materializes_once_after_both_pulls() {
    let content = b"shared legacy bytes".to_vec();
    let blob_hash = hash_bytes(&content);
    let executable_content = b"#!/bin/sh\nexit 0\n".to_vec();
    let executable_hash = hash_bytes(&executable_content);
    let empty_hash = hash_bytes(b"");
    let tree = Tree::new(vec![
        TreeEntry {
            name: "shared.txt".to_string(),
            kind: TreeEntryKind::Blob,
            hash: blob_hash.clone(),
            mode: FileMode::Regular,
        },
        TreeEntry {
            name: "tool.sh".to_string(),
            kind: TreeEntryKind::Blob,
            hash: executable_hash.clone(),
            mode: FileMode::Executable,
        },
        TreeEntry {
            name: "empty.txt".to_string(),
            kind: TreeEntryKind::Blob,
            hash: empty_hash.clone(),
            mode: FileMode::Regular,
        },
    ])
    .unwrap();
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
    let branch = |name: &str, parent: Option<&str>| {
        serde_json::json!({
            "name": name,
            "description": null,
            "parent_branch": parent,
            "status": "open",
            "created_at": "2026-01-01T00:00:00Z"
        })
    };
    let blob = serde_json::json!({
        "hash": blob_hash,
        "content": [],
        "size": content.len(),
        "chunks": [{"hash": blob_hash, "offset": 0, "size": content.len()}]
    });
    let executable_blob = serde_json::json!({
        "hash": executable_hash,
        "content": [],
        "size": executable_content.len(),
        "chunks": [{"hash": executable_hash, "offset": 0, "size": executable_content.len()}]
    });
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "credential_presented": true,
            "credential_accepted": true
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
            "snapshot_token": "initial-only-snapshot",
            "scope": {"commit_count": 2, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("branch_name", "feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": feature.hash,
            "branch": branch("feature", Some("main")),
            "branches": [branch("main", None), branch("feature", Some("main"))],
            "commits": [commit_wire(&feature)],
            "blobs": [blob.clone(), executable_blob.clone()],
            "trees": [tree_to_wire(&tree)],
            "renames": [],
            "restricted_blobs": [],
            "missing_content": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": main.hash,
            "branch": branch("main", None),
            "branches": [branch("main", None), branch("feature", Some("main"))],
            "commits": [commit_wire(&main)],
            "blobs": [blob, executable_blob],
            "trees": [tree_to_wire(&tree)],
            "renames": [],
            "restricted_blobs": [],
            "missing_content": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [
                {"hash": blob_hash, "content": content},
                {"hash": executable_hash, "content": executable_content}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--allow-legacy-scope",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_API_KEY", "legacy-token")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid receipt {error}: {:?}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(
        receipt["result"]["acquisition_profile"],
        "bounded_v1_selected_branch_unbound"
    );
    assert_eq!(receipt["result"]["preflight_profile"], "bounded_v1");
    assert_eq!(receipt["result"]["snapshot_bound"], false);
    assert_eq!(receipt["result"]["initial_pull_snapshot_bound"], true);
    assert_eq!(receipt["result"]["selected_branch_snapshot_bound"], false);
    assert_eq!(
        receipt["result"]["proof_scope"],
        "initial_pull_only_selected_branch_unbound"
    );
    assert_eq!(
        receipt["result"]["preflight_credential_disposition"],
        "server_reported_accepted"
    );
    assert_eq!(
        receipt["observed"]["pull_phases"].as_array().unwrap().len(),
        2
    );
    assert_eq!(receipt["observed"]["object_entries_received"]["commits"], 2);
    assert_eq!(receipt["observed"]["object_entries_received"]["trees"], 2);
    assert_eq!(
        receipt["observed"]["object_entries_received"]["blob_descriptors"],
        4
    );
    assert_eq!(receipt["observed"]["chunks"]["required_unique"], 2);
    assert_eq!(receipt["observed"]["chunks"]["downloaded_unique"], 2);
    assert_eq!(
        receipt["observed"]["chunks"]["present_at_clone_start_unique"],
        0
    );
    assert_eq!(
        receipt["observed"]["chunks"]["reused_from_prior_phase_unique"],
        2
    );
    assert_eq!(receipt["observed"]["materialization"]["passes_observed"], 1);
    assert_eq!(receipt["observed"]["materialization"]["written_paths"], 3);
    assert_eq!(
        std::fs::read(destination.join("shared.txt")).unwrap(),
        content
    );
    assert_eq!(
        std::fs::read(destination.join("tool.sh")).unwrap(),
        executable_content
    );
    assert_eq!(std::fs::read(destination.join("empty.txt")).unwrap(), b"");
    assert_valid_stat_cache(
        &destination,
        &[
            ("empty.txt", &empty_hash),
            ("shared.txt", &blob_hash),
            ("tool.sh", &executable_hash),
        ],
    );
    let hash = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .arg("hash")
        .current_dir(&destination)
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(hash.status.success());
    assert_eq!(
        String::from_utf8_lossy(&hash.stdout).trim(),
        feature.hash.as_str()
    );
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["status", "--json"])
        .current_dir(&destination)
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["branch"], "feature");
    assert_eq!(status["head"], feature.hash.as_str());
    assert_eq!(status["working_changes"]["changes"], serde_json::json!([]));
    let mut same_size_edit = content.clone();
    same_size_edit[0] ^= 0x20;
    std::fs::write(destination.join("shared.txt"), &same_size_edit).unwrap();
    let changed = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["status", "--json"])
        .current_dir(&destination)
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(changed.status.success());
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(
        changed["working_changes"]["changes"][0]["path"],
        "shared.txt"
    );
    let commit = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .arg("commit")
        .current_dir(&destination)
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let changed_head = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .arg("hash")
        .current_dir(&destination)
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(changed_head.status.success());
    assert_ne!(
        String::from_utf8_lossy(&changed_head.stdout).trim(),
        feature.hash.as_str()
    );
    assert!(!destination.join(".oak/clone_in_progress").exists());
}

#[tokio::test]
async fn legacy_selected_clone_preserves_unresolvable_409_and_cleans_destination() {
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
    let branch = |name: &str, parent: Option<&str>| {
        serde_json::json!({
            "name": name,
            "description": null,
            "parent_branch": parent,
            "status": "open",
            "created_at": "2026-01-01T00:00:00Z"
        })
    };
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
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
            "snapshot_token": "initial-only-snapshot",
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("branch_name", "feature"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": main.hash,
            "branch": branch("main", None),
            "branches": [branch("main", None), branch("feature", Some("main"))],
            "commits": [commit_wire(&main)],
            "blobs": [],
            "trees": [],
            "renames": [],
            "restricted_blobs": [],
            "missing_content": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--allow-legacy-scope",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Run 'oak pull' to converge"));
    assert!(!destination.exists());
}

#[tokio::test]
async fn clone_json_rejects_git_adapter_before_destination_or_network_work() {
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("git-destination");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "https://127.0.0.1:9/repo.git",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid error envelope {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(error["schema_version"], 1);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("clone --json is only supported for Oak repositories"));
    assert!(!destination.exists());
}

#[tokio::test]
async fn clone_json_rejects_interactive_picker_before_remote_work() {
    let home = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["clone", "--json", "--remote", "http://127.0.0.1:9"])
        .env("HOME", home.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("requires ORG/REPO"));
}

#[tokio::test]
async fn clone_json_malformed_pull_is_one_error_document_and_cleans_destination() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"repo"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{not json", "application/json"))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--allow-legacy-scope",
            "--remote",
            &server.uri(),
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
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Failed to parse server response"));
    assert!(!destination.exists());
}

#[tokio::test]
async fn clone_json_reports_legacy_headless_repository_without_fabricating_identity() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name":"repo"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head":null,"branch":null,"branches":[],"commits":[],"blobs":[],"trees":[],
            "renames":[],"restricted_blobs":[],"missing_content":[]
        })))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        receipt["result"]["acquisition_profile"],
        "legacy_unverified"
    );
    assert_eq!(receipt["result"]["head"], serde_json::Value::Null);
    assert_eq!(receipt["result"]["manifest"], serde_json::Value::Null);
    assert_eq!(receipt["result"]["snapshot_bound"], false);
    assert_eq!(receipt["result"]["initial_pull_snapshot_bound"], false);
    assert_eq!(
        receipt["result"]["selected_branch_snapshot_bound"],
        serde_json::Value::Null
    );
    assert!(destination.join(".oak/oak.db").exists());
}

#[tokio::test]
async fn clone_json_identifies_explicit_budget_override_without_claiming_complete_proof() {
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
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "content_incomplete",
            "healthy": false,
            "complete": false,
            "truncated": true,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "budget-snapshot",
            "selected_branch": {"name": "feature", "head": feature.hash, "status": "open"},
            "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "findings": [{
                "code": "clone_preflight_history_budget_exhausted",
                "detail": "bounded walk stopped",
                "recoverability": "unknown",
                "affected": []
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": feature.hash,
            "branch": null,
            "branches": [{"name":"feature","description":null,"parent_branch":null,"status":"open","created_at":"2026-01-01T00:00:00Z"}],
            "commits": [commit_wire(&feature)],
            "blobs": [], "trees": [], "renames": [], "restricted_blobs": [], "missing_content": []
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--allow-unverified-integrity",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env("OAK_API_KEY", "rejected-preflight-token")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        receipt["result"]["acquisition_profile"],
        "bounded_v1_budget_override"
    );
    assert_eq!(receipt["result"]["snapshot_bound"], true);
    assert_eq!(
        receipt["result"]["preflight_credential_disposition"],
        "server_reported_not_accepted"
    );
    assert_eq!(
        receipt["result"]["proof_scope"],
        "snapshot_bound_budget_incomplete"
    );
}

#[tokio::test]
async fn sparse_clone_json_counts_only_observed_manifest_dispositions() {
    let in_bytes = b"inside cone".to_vec();
    let in_hash = hash_bytes(&in_bytes);
    let out_hash = hash_bytes(b"withheld outside cone");
    let tree = Tree::new(vec![
        TreeEntry {
            name: "in.txt".into(),
            kind: TreeEntryKind::Blob,
            hash: in_hash.clone(),
            mode: FileMode::Regular,
        },
        TreeEntry {
            name: "out.txt".into(),
            kind: TreeEntryKind::Blob,
            hash: out_hash,
            mode: FileMode::Regular,
        },
    ])
    .unwrap();
    let feature = Commit::with_timestamp(
        "feature".into(),
        None,
        None,
        tree.hash.clone(),
        "tester".into(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1, "clone_preflight_profile":"bounded_v1",
            "sparse_materialization_protocol":"cone_v1",
            "selected_branch_acquisition":"exact_head_v1",
            "credential_presented":false, "credential_accepted":false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("paths", "in.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"repo":"oak/repo","status":"healthy","healthy":true,
            "complete":true,"truncated":false,"verification":"metadata","proof_profile":"bounded_v1",
            "sparse_materialization_protocol":"cone_v1","snapshot_token":"sparse-snapshot",
            "selected_branch":{"name":"feature","head":feature.hash,"status":"open"},
            "scope":{"commit_count":1,"manifest_count":1,"blob_count":1,"chunk_count":1},"findings":[]
        }))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("paths", "in.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head":feature.hash,"branch":null,
            "branches":[{"name":"feature","description":null,"parent_branch":null,"status":"open","created_at":"2026-01-01T00:00:00Z"}],
            "commits":[commit_wire(&feature)],
            "blobs":[{"hash":in_hash,"content":[],"size":in_bytes.len(),"chunks":[{"hash":in_hash,"offset":0,"size":in_bytes.len()}]}],
            "trees":[tree_to_wire(&tree)],"renames":[],"restricted_blobs":[],"missing_content":[]
        }))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks":[{"hash":in_hash,"content":in_bytes}]
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--remote",
            &server.uri(),
            "--branch",
            "feature",
            "--path",
            "in.txt",
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["request"]["materialization"]["mode"], "sparse");
    assert_eq!(
        receipt["observed"]["materialization"]["path_counters_scope"],
        "final_clone_materialization"
    );
    assert_eq!(
        receipt["observed"]["materialization"]["observed_eligible_paths"],
        1
    );
    assert_eq!(
        receipt["observed"]["materialization"]["observed_out_of_cone_paths"],
        1
    );
    assert_eq!(receipt["observed"]["materialization"]["written_paths"], 1);
    assert!(destination.join("in.txt").exists());
    assert!(!destination.join("out.txt").exists());
    assert_valid_stat_cache(&destination, &[("in.txt", &in_hash)]);
}

#[tokio::test]
async fn clone_json_reports_historical_known_loss_without_calling_it_materialized_absence() {
    let lost_hash = hash_bytes(b"lost history");
    let historical_tree = Tree::new(vec![TreeEntry {
        name: "old.txt".into(),
        kind: TreeEntryKind::Blob,
        hash: lost_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let healthy_bytes = b"visible".to_vec();
    let healthy_hash = hash_bytes(&healthy_bytes);
    let current_tree = Tree::new(vec![TreeEntry {
        name: "visible.txt".into(),
        kind: TreeEntryKind::Blob,
        hash: healthy_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let timestamp = |value| {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    let historical = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        historical_tree.hash.clone(),
        "tester".into(),
        None,
        Vec::new(),
        timestamp("2026-01-01T00:00:00Z"),
    )
    .unwrap();
    let current = Commit::with_timestamp(
        "main".into(),
        Some(historical.hash.clone()),
        None,
        current_tree.hash.clone(),
        "tester".into(),
        None,
        Vec::new(),
        timestamp("2026-01-01T00:00:01Z"),
    )
    .unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"clone_preflight_profile":"bounded_v1",
            "credential_presented":false,"credential_accepted":false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"repo":"oak/repo","status":"content_incomplete","healthy":false,
            "complete":true,"truncated":false,"verification":"metadata","proof_profile":"bounded_v1",
            "known_loss_protocol":"report_v1","snapshot_token":"loss-snapshot",
            "scope":{"commit_count":2,"manifest_count":2,"blob_count":2,"chunk_count":1},
            "findings":[{"code":"known_lost_blob","blob_hash":lost_hash,
                "recoverability":"operator_adjudicated_loss","detail":"historical bytes unavailable"}],
            "head_affected":false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head":current.hash,"branch":null,
            "branches":[{"name":"main","description":null,"parent_branch":null,"status":"open","created_at":"2026-01-01T00:00:00Z"}],
            "commits":[commit_wire(&historical),commit_wire(&current)],
            "blobs":[{"hash":healthy_hash,"content":[],"size":healthy_bytes.len(),
                "chunks":[{"hash":healthy_hash,"offset":0,"size":healthy_bytes.len()}]}],
            "trees":[tree_to_wire(&historical_tree),tree_to_wire(&current_tree)],
            "renames":[],"restricted_blobs":[],
            "missing_content":[{"kind":"blob","hash":lost_hash,"reason_code":"operator_adjudicated_loss"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks":[{"hash":healthy_hash,"content":healthy_bytes}]
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        receipt["result"]["acquisition_profile"],
        "bounded_v1_known_loss"
    );
    assert_eq!(
        receipt["result"]["proof_scope"],
        "requested_history_with_adjudicated_historical_loss"
    );
    assert_eq!(
        receipt["observed"]["materialization"]["restricted_paths"],
        0
    );
    assert_eq!(
        receipt["observed"]["materialization"]["known_lost_paths"],
        0
    );
    assert!(destination.join("visible.txt").exists());
    assert!(!destination.join("old.txt").exists());
    assert_valid_stat_cache(&destination, &[("visible.txt", &healthy_hash)]);
}

#[tokio::test]
async fn clone_json_reports_current_permission_withholding_separately() {
    let restricted_hash = hash_bytes(b"not authorized");
    let tree = Tree::new(vec![TreeEntry {
        name: "private.txt".into(),
        kind: TreeEntryKind::Blob,
        hash: restricted_hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let main = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        tree.hash.clone(),
        "tester".into(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"clone_preflight_profile":"bounded_v1",
            "credential_presented":false,"credential_accepted":false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"repo":"oak/repo","status":"healthy","healthy":true,
            "complete":true,"truncated":false,"verification":"metadata","proof_profile":"bounded_v1",
            "snapshot_token":"permission-snapshot",
            "scope":{"commit_count":1,"manifest_count":1,"blob_count":1,"chunk_count":0},"findings":[]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head":main.hash,"branch":null,
            "branches":[{"name":"main","description":null,"parent_branch":null,"status":"open","created_at":"2026-01-01T00:00:00Z"}],
            "commits":[commit_wire(&main)],"blobs":[],"trees":[tree_to_wire(&tree)],
            "renames":[],"restricted_blobs":[restricted_hash],"missing_content":[]
        })))
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["result"]["acquisition_profile"], "bounded_v1");
    assert_eq!(
        receipt["observed"]["materialization"]["restricted_paths"],
        1
    );
    assert_eq!(
        receipt["observed"]["materialization"]["missing_required_paths"],
        0
    );
    assert!(!destination.join("private.txt").exists());
    assert_valid_stat_cache(&destination, &[]);
}

#[tokio::test]
async fn clone_json_reports_explicit_partial_recovery_as_missing_required_path() {
    let missing_hash = hash_bytes(b"server omitted these bytes");
    let tree = Tree::new(vec![TreeEntry {
        name: "missing.txt".into(),
        kind: TreeEntryKind::Blob,
        hash: missing_hash,
        mode: FileMode::Regular,
    }])
    .unwrap();
    let main = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        tree.hash.clone(),
        "tester".into(),
        None,
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"clone_preflight_profile":"bounded_v1",
            "credential_presented":false,"credential_accepted":false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version":1,"repo":"oak/repo","status":"healthy","healthy":true,
            "complete":true,"truncated":false,"verification":"metadata","proof_profile":"bounded_v1",
            "snapshot_token":"partial-snapshot",
            "scope":{"commit_count":1,"manifest_count":1,"blob_count":1,"chunk_count":0},"findings":[]
        }))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head":main.hash,"branch":null,
            "branches":[{"name":"main","description":null,"parent_branch":null,"status":"open","created_at":"2026-01-01T00:00:00Z"}],
            "commits":[commit_wire(&main)],"blobs":[],"trees":[tree_to_wire(&tree)],
            "renames":[],"restricted_blobs":[],"missing_content":[]
        }))).mount(&server).await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "clone",
            "--json",
            "--remote",
            &server.uri(),
            "oak/repo",
            destination.to_str().unwrap(),
        ])
        .env("HOME", home.path())
        .env_remove("OAK_API_KEY")
        .env("OAK_ALLOW_PARTIAL_CLONE", "1")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        receipt["observed"]["materialization"]["missing_required_paths"],
        1
    );
    assert_eq!(
        receipt["observed"]["materialization"]["restricted_paths"],
        0
    );
    assert_eq!(
        receipt["observed"]["materialization"]["known_lost_paths"],
        0
    );
    assert!(!destination.join("missing.txt").exists());
    assert_valid_stat_cache(&destination, &[]);
}
