//! HTTP-level tests for the mount remote helpers, using wiremock so we
//! don't need a running oak-server.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use oak_cli::commands::mount::remote;
use oak_core::SqliteRepository;
use tempfile::TempDir;

/// A throwaway on-disk cache for the blob-size helper to persist chunk refs.
fn temp_cache() -> (TempDir, SqliteRepository) {
    let dir = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&dir.path().join("cache.db")).unwrap();
    (dir, repo)
}

#[tokio::test]
async fn resolve_branch_head_explicit_branch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo/branches/feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "feature",
            "description": null,
            "parent_branch": "main",
            "status": "open",
            "created_at": "2026-04-01T00:00:00Z",
            "head": "abc123def456"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (branch, head) =
        remote::resolve_branch_head(&server.uri(), "oak", "myrepo", Some("feature"), None)
            .await
            .expect("should resolve explicit branch");
    assert_eq!(branch, "feature");
    assert_eq!(head, "abc123def456");
}

#[tokio::test]
async fn resolve_branch_head_default_picks_main() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "myrepo",
            "head": "head-hash",
            "is_public": true,
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "branches": [
                {"name": "feature-x", "head": "head-hash", "created_at": "2026-04-01T00:00:00Z", "status": "open"},
                {"name": "main", "head": "head-hash", "created_at": "2026-04-01T00:00:00Z", "status": "open"},
                {"name": "old", "head": "old-hash", "created_at": "2026-04-01T00:00:00Z", "status": "open"},
            ]
        })))
        .mount(&server)
        .await;

    let (branch, head) = remote::resolve_branch_head(&server.uri(), "oak", "myrepo", None, None)
        .await
        .expect("should resolve default branch");
    // Out of {feature-x, main} (both at head-hash), main wins.
    assert_eq!(branch, "main");
    assert_eq!(head, "head-hash");
}

#[tokio::test]
async fn resolve_branch_head_missing_branch_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/myrepo/branches/ghost"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error":"not found"})))
        .mount(&server)
        .await;

    let err = remote::resolve_branch_head(&server.uri(), "oak", "myrepo", Some("ghost"), None)
        .await
        .expect_err("missing branch should fail");
    let msg = err.to_string();
    assert!(msg.contains("ghost"), "error should name branch: {msg}");
}

#[tokio::test]
async fn fetch_blob_sizes_batches_and_collects() {
    let server = MockServer::start().await;
    let hashes: Vec<String> = (0..5).map(|i| format!("blob-{i:02}")).collect();

    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/blobs/info"))
        .and(body_partial_json(json!({"hashes": hashes})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": hashes.iter().enumerate().map(|(i, h)| json!({
                "hash": h,
                "content": [],
                "size": (i as u64) * 100,
            })).collect::<Vec<_>>()
        })))
        .expect(1)
        .mount(&server)
        .await;

    let (_dir, cache) = temp_cache();
    let sizes = remote::fetch_blob_sizes(&cache, &server.uri(), "oak", "myrepo", &hashes, None)
        .await
        .expect("should fetch sizes");
    assert_eq!(sizes.len(), 5);
    let map: std::collections::HashMap<String, u64> = sizes.into_iter().collect();
    assert_eq!(map.get("blob-00").copied(), Some(0));
    assert_eq!(map.get("blob-04").copied(), Some(400));
}

#[tokio::test]
async fn fetch_blob_sizes_empty_input_is_noop() {
    // No mock at all — should not hit the server.
    let server = MockServer::start().await;
    let (_dir, cache) = temp_cache();
    let sizes = remote::fetch_blob_sizes(&cache, &server.uri(), "oak", "myrepo", &[], None)
        .await
        .expect("empty fetch is ok");
    assert!(sizes.is_empty());
}

#[tokio::test]
async fn fetch_commit_into_cache_stores_commit_and_manifest() {
    use oak_core::Hash;
    use oak_core::{Repository, SqliteRepository};
    use tempfile::TempDir;

    let server = MockServer::start().await;
    // Full-length hex: tree entries and hash-position wire fields are
    // validated as real hashes now, so abbreviated placeholders are rejected.
    let commit_hash = "deadbeef".repeat(8);
    let manifest_hash = "abcd1234".repeat(8);
    let blob_hash = "f00f".repeat(16);

    Mock::given(method("POST"))
        .and(path("/api/oak/myrepo/commits/info"))
        .and(body_partial_json(json!({"hashes": [&commit_hash]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": commit_hash,
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": manifest_hash,
                "author": "alice",
                "message": "initial",
                "timestamp": "2026-04-01T12:00:00Z",
                "files": []
            }],
            // Server response shape switched from `manifests:` (flat
            // path -> blob list) to `trees:` (per-directory entries with
            // kind/mode). See `oak_server::api::repos::TreeData`.
            "trees": [{
                "hash": manifest_hash,
                "entries": [{
                    "name": "README.md",
                    "kind": "blob",
                    "hash": blob_hash,
                    "mode": "regular"
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let temp = TempDir::new().unwrap();
    // Mount uses `open_relaxed` so manifest entries can reference blobs
    // we haven't fetched yet. Mirror that here.
    let cache = SqliteRepository::open_relaxed(&temp.path().join("cache.db")).unwrap();

    // Seed the base branch so the commit's FK is satisfied. `start()`
    // does this before fetching commits for the same reason.
    cache
        .store_branch(&oak_core::Branch::new("main".into(), None, None))
        .unwrap();

    let head = Hash(commit_hash.clone());
    remote::fetch_commit_into_cache(&cache, &server.uri(), "oak", "myrepo", &head, None)
        .await
        .expect("should populate cache");

    // Commit + its manifest should now be in the local cache, but no
    // blobs (mount only fetches blobs lazily).
    let commit = cache.get_commit(&head).unwrap().expect("commit stored");
    assert_eq!(commit.message, Some("initial".to_string()));
    assert_eq!(commit.manifest_hash.as_str(), manifest_hash);

    let manifest = cache
        .get_manifest(&commit.manifest_hash)
        .unwrap()
        .expect("manifest stored");
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].path, "README.md");
    assert_eq!(manifest.entries[0].blob_hash.as_str(), blob_hash);

    assert!(
        cache.get_blob(&Hash(blob_hash)).unwrap().is_none(),
        "blob should NOT be downloaded by fetch_commit_into_cache"
    );
}
