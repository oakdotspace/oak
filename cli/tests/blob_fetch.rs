//! HTTP-level tests for `blob_fetch::ensure_blobs_local` using a mock server.
//!
//! Exercises the real reqwest client against wiremock-served responses so
//! the JSON shapes, concurrency, and error paths are verified end-to-end
//! without spinning up the full oak-server (which requires Postgres).

use std::path::Path;

use oak_core::{hash_bytes, Commit, Hash};
use oak_core::{Repository, SqliteRepository};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn init_repo(dir: &Path) {
    // Non-interactive: tests call init in-process, so a real stdin TTY must
    // not trigger the optional setup prompts (CLAUDE.md, etc.).
    oak_cli::commands::init::run(dir, false).unwrap();
}

fn open_repo(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

#[tokio::test]
async fn test_ensure_blobs_local_fetches_inline_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // A small blob is represented as a single self-chunk on the wire.
    let content = b"hello pinned dep".to_vec();
    let blob_hash = hash_bytes(&content);
    assert!(!repo.has_blob(&blob_hash).unwrap(), "precondition");
    repo.set_metadata(oak_core::MetadataKey::KnownLostBlobs, blob_hash.as_str())
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .and(body_partial_json(json!({ "hashes": [blob_hash.as_str()] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": blob_hash.as_str(),
                "size": content.len(),
                "chunks": [{
                    "hash": blob_hash.as_str(),
                    "offset": 0,
                    "size": content.len(),
                }],
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .and(body_partial_json(json!({ "hashes": [blob_hash.as_str()] })))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_string("admission busy"),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .and(body_partial_json(json!({ "hashes": [blob_hash.as_str()] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": blob_hash.as_str(),
                "content": content,
            }]
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect("ensure_blobs_local should succeed");

    let stored = repo.get_blob(&blob_hash).unwrap().expect("blob stored");
    assert_eq!(stored.content, content);
    assert_eq!(stored.size, content.len() as u64);
    assert!(
        oak_cli::commands::known_loss::load_known_lost_blobs(&repo).is_empty(),
        "a byte-verified, durably stored blob must no longer remain marked lost"
    );
}

#[tokio::test]
async fn test_ensure_blobs_local_fetches_chunked_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // A deterministic two-chunk blob. The client only needs chunk metadata
    // plus chunk bytes; FastCDC behavior is covered in oak-core tests.
    let content = b"hello chunked blob".to_vec();
    let blob_hash = hash_bytes(&content);
    let first = b"hello ".to_vec();
    let second = b"chunked blob".to_vec();
    let chunks = vec![
        (
            oak_core::ChunkInfo {
                hash: hash_bytes(&first),
                offset: 0,
                length: first.len() as u32,
            },
            first,
        ),
        (
            oak_core::ChunkInfo {
                hash: hash_bytes(&second),
                offset: 6,
                length: second.len() as u32,
            },
            second,
        ),
    ];

    // Build the /blobs/info response shape and a hash→bytes map for
    // serving chunk downloads inline.
    let mut info_chunks: Vec<serde_json::Value> = Vec::new();
    let mut chunk_bytes: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    for (info, data) in &chunks {
        info_chunks.push(json!({
            "hash": info.hash.as_str(),
            "offset": info.offset,
            "size": info.length,
        }));
        chunk_bytes.insert(info.hash.as_str().to_string(), data.clone());
    }

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": blob_hash.as_str(),
                "content": Vec::<u8>::new(),
                "size": content.len(),
                "chunks": info_chunks,
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    // /chunks/download — return each requested chunk inline.
    let chunk_bytes_clone = chunk_bytes.clone();
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(move |req: &wiremock::Request| {
            #[derive(serde::Deserialize)]
            struct Req {
                hashes: Vec<String>,
            }
            let body: Req = serde_json::from_slice(&req.body).unwrap();
            let chunks: Vec<serde_json::Value> = body
                .hashes
                .iter()
                .map(|h| {
                    json!({
                        "hash": h,
                        "content": chunk_bytes_clone.get(h).cloned().unwrap_or_default(),
                    })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "chunks": chunks }))
        })
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect("ensure_blobs_local should succeed");

    let stored = repo.get_blob(&blob_hash).unwrap().expect("blob stored");
    assert_eq!(stored.content, content, "reassembled content must match");
    // Chunk metadata should also be stored for future dedup.
    let chunk_infos = repo.get_blob_chunks(&blob_hash).unwrap();
    let stored_chunks = chunk_infos.expect("chunks metadata stored");
    assert_eq!(stored_chunks.len(), chunks.len());
}

#[tokio::test]
async fn test_ensure_blobs_local_short_circuits_when_all_local() {
    // If every requested blob is already local, no HTTP calls should happen.
    // We assert this by pointing the client at a server whose handlers would
    // panic if hit (no mocks mounted — wiremock returns 404 on unmatched).
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let content = b"already here".to_vec();
    let blob = oak_core::Blob::new(content.clone());
    let blob_hash = blob.hash.clone();
    repo.store_blob(&blob).unwrap();

    // Server with no mocks: any request would 404, but we expect no request.
    let server = MockServer::start().await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect("should succeed without any HTTP calls");

    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn test_ensure_blobs_local_skips_info_when_refs_cached() {
    // When a blob's chunk refs are already cached locally (as `oak mount`
    // seeds them at mount time), a cold fetch must skip the `/blobs/info`
    // round-trip and download chunks directly — halving cold read latency.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let content = b"refs known up front".to_vec();
    let blob_hash = hash_bytes(&content);
    // Seed the blob→chunk mapping without the blob bytes or chunk bytes — this
    // is only possible because migration 0009 relaxed the blob_chunks FK.
    repo.store_blob_chunks(
        &blob_hash,
        &[oak_core::ChunkInfo {
            hash: blob_hash.clone(),
            offset: 0,
            length: content.len() as u32,
        }],
    )
    .unwrap();
    assert!(!repo.has_blob(&blob_hash).unwrap(), "precondition");

    let server = MockServer::start().await;
    // /blobs/info must NOT be hit — expect(0) fails the test on any call.
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .and(body_partial_json(json!({ "hashes": [blob_hash.as_str()] })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{ "hash": blob_hash.as_str(), "content": content }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect("ensure_blobs_local should succeed via cached refs");

    let stored = repo.get_blob(&blob_hash).unwrap().expect("blob stored");
    assert_eq!(stored.content, content);
}

#[tokio::test]
async fn test_ensure_blobs_local_errors_when_remote_missing_blob() {
    // Caller asks for a blob that doesn't exist on the remote. The server
    // reports an empty `blobs` list; the client should surface a useful error.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let missing_hash = Hash::from_hex(&"a".repeat(64)).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "blobs": [] })))
        .mount(&server)
        .await;

    let err = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&missing_hash),
    )
    .await
    .expect_err("should error on missing blob");
    let msg = err.to_string();
    assert!(
        msg.contains(missing_hash.as_str()) || msg.contains("missing"),
        "expected missing-blob error, got: {msg}"
    );
}

#[tokio::test]
async fn test_ensure_blobs_local_rejects_wrong_blob_set_before_local_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let requested = hash_bytes(b"requested blob");
    let unrequested_content = b"unrequested blob".to_vec();
    let unrequested = hash_bytes(&unrequested_content);
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": unrequested.as_str(),
                "size": unrequested_content.len(),
                "chunks": [{
                    "hash": unrequested.as_str(),
                    "offset": 0,
                    "size": unrequested_content.len(),
                }],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": unrequested.as_str(),
                "content": unrequested_content,
            }]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested),
    )
    .await
    .expect_err("same-cardinality wrong blob response must fail");

    assert!(error.to_string().contains("unrequested"), "got: {error}");
    assert!(!repo.has_blob(&requested).unwrap());
    assert!(!repo.has_blob(&unrequested).unwrap());
    assert!(repo.get_blob_chunks(&unrequested).unwrap().is_none());
    assert!(repo.get_chunk(&unrequested).unwrap().is_none());
}

#[tokio::test]
async fn test_ensure_blobs_local_rejects_duplicate_blob_before_local_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let content = b"requested blob".to_vec();
    let requested = hash_bytes(&content);
    let duplicate = json!({
        "hash": requested.as_str(),
        "size": content.len(),
        "chunks": [{
            "hash": requested.as_str(),
            "offset": 0,
            "size": content.len(),
        }],
    });
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [duplicate.clone(), duplicate]
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested),
    )
    .await
    .expect_err("duplicate blob response must fail");

    assert!(error.to_string().contains("duplicate"), "got: {error}");
    assert!(!repo.has_blob(&requested).unwrap());
    assert!(repo.get_blob_chunks(&requested).unwrap().is_none());
    assert!(repo.get_chunk(&requested).unwrap().is_none());
}

#[tokio::test]
async fn test_legacy_full_branch_superset_cannot_mask_missing_requested_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let requested = hash_bytes(b"requested legacy blob");
    let unrelated_content = b"unrelated branch blob".to_vec();
    let unrelated = hash_bytes(&unrelated_content);
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/testowner/demo/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "branch": { "name": "main" },
            "blobs": [{
                "hash": unrelated.as_str(),
                "content": unrelated_content,
                "size": 21,
                "chunks": [],
            }],
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local_for_legacy_push(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        "main",
        None,
        std::slice::from_ref(&requested),
    )
    .await
    .expect_err("an unrelated full-branch superset must not prove the requested blob");

    assert!(
        error.to_string().contains("omitted requested"),
        "got: {error}"
    );
    assert!(!repo.has_blob(&requested).unwrap());
    assert!(!repo.has_blob(&unrelated).unwrap());
    assert!(repo.get_blob_chunks(&unrelated).unwrap().is_none());
}

#[tokio::test]
async fn test_chunk_download_rejects_wrong_hash_before_local_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let requested_content = b"requested chunk".to_vec();
    let requested_chunk = hash_bytes(&requested_content);
    let requested_blob = requested_chunk.clone();
    let unrequested_content = b"unrequested chunk".to_vec();
    let unrequested_chunk = hash_bytes(&unrequested_content);
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": requested_blob.as_str(),
                "size": requested_content.len(),
                "chunks": [{
                    "hash": requested_chunk.as_str(),
                    "offset": 0,
                    "size": requested_content.len(),
                }],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": unrequested_chunk.as_str(),
                "content": unrequested_content,
            }]
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested_blob),
    )
    .await
    .expect_err("same-cardinality wrong chunk response must fail");

    assert!(error.to_string().contains("unrequested"), "got: {error}");
    assert!(repo.get_chunk(&requested_chunk).unwrap().is_none());
    assert!(
        repo.get_chunk(&unrequested_chunk).unwrap().is_none(),
        "response validation must precede chunk persistence"
    );
    assert!(
        repo.get_blob_chunks(&requested_blob).unwrap().is_none(),
        "a failed chunk response must not leave an unverified mapping"
    );
    assert!(!repo.has_blob(&requested_blob).unwrap());
}

#[tokio::test]
async fn test_blob_mapping_rejects_wrong_initial_offset_before_download_or_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let first = b"first".to_vec();
    let second = b"second".to_vec();
    let first_hash = hash_bytes(&first);
    let second_hash = hash_bytes(&second);
    let mut content = first.clone();
    content.extend_from_slice(&second);
    let blob_hash = hash_bytes(&content);
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": blob_hash.as_str(),
                "size": content.len(),
                "chunks": [
                    { "hash": first_hash.as_str(), "offset": 1, "size": first.len() },
                    { "hash": second_hash.as_str(), "offset": 1 + first.len(), "size": second.len() },
                ],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect_err("a mapping that does not begin at zero must fail before download");

    assert!(error.to_string().contains("offset"), "got: {error}");
    assert!(!repo.has_blob(&blob_hash).unwrap());
    assert!(repo.get_blob_chunks(&blob_hash).unwrap().is_none());
    assert!(repo.get_chunk(&first_hash).unwrap().is_none());
    assert!(repo.get_chunk(&second_hash).unwrap().is_none());
}

#[tokio::test]
async fn test_blob_mapping_size_failure_rolls_back_prior_blob_and_chunks() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let first = b"first valid blob".to_vec();
    let second = b"second blob".to_vec();
    let first_hash = hash_bytes(&first);
    let second_hash = hash_bytes(&second);
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [
                {
                    "hash": first_hash.as_str(),
                    "size": first.len(),
                    "chunks": [{
                        "hash": first_hash.as_str(), "offset": 0, "size": first.len()
                    }],
                },
                {
                    "hash": second_hash.as_str(),
                    "size": second.len() + 1,
                    "chunks": [{
                        "hash": second_hash.as_str(), "offset": 0, "size": second.len() + 1
                    }],
                },
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [
                { "hash": first_hash.as_str(), "content": first },
                { "hash": second_hash.as_str(), "content": second },
            ]
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        &[first_hash.clone(), second_hash.clone()],
    )
    .await
    .expect_err("declared chunk length must equal fetched plaintext length");

    assert!(error.to_string().contains("size"), "got: {error}");
    for hash in [&first_hash, &second_hash] {
        assert!(
            !repo.has_blob(hash).unwrap(),
            "no blob may survive rollback"
        );
        assert!(repo.get_blob_chunks(hash).unwrap().is_none());
        assert!(repo.get_chunk(hash).unwrap().is_none());
    }
}

#[tokio::test]
async fn test_ensure_commits_local_fetches_missing_commit() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // Use an empty manifest to keep the commit payload self-contained —
    // SQLite FKs require referenced blobs to exist if we include entries.
    let manifest_hash = oak_core::Manifest::empty().hash;
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let commit_hash = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        manifest_hash.clone(),
        "tester".to_string(),
        Some("seed".to_string()),
        Vec::new(),
        timestamp,
    )
    .unwrap()
    .hash;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/commits/info"))
        .and(body_partial_json(
            json!({ "hashes": [commit_hash.as_str()] }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": commit_hash.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": manifest_hash.as_str(),
                "author": "tester",
                "message": "seed",
                "timestamp": "2026-04-23T10:00:00Z",
                "files": [],
            }],
            "manifests": [{
                "hash": manifest_hash.as_str(),
                "entries": [],
            }],
        })))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_commits_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&commit_hash),
    )
    .await
    .expect("fetch should succeed");

    let commit = repo.get_commit(&commit_hash).unwrap().expect("stored");
    assert_eq!(commit.branch_name, "main");
    assert_eq!(commit.manifest_hash, manifest_hash);

    let manifest = repo.get_manifest(&manifest_hash).unwrap().expect("stored");
    assert_eq!(manifest.entries.len(), 0);
}

#[tokio::test]
async fn test_ensure_commits_local_accepts_omitted_canonical_empty_child_tree() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let root = oak_core::Tree::new(vec![oak_core::TreeEntry {
        name: "empty-directory".to_string(),
        kind: oak_core::TreeEntryKind::Tree,
        hash: oak_core::Tree::empty_hash(),
        mode: oak_core::FileMode::Regular,
    }])
    .unwrap();
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let commit = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        root.hash.clone(),
        "tester".to_string(),
        Some("empty child".to_string()),
        Vec::new(),
        timestamp,
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": commit.hash.as_str(),
                "branch_name": commit.branch_name,
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": commit.manifest_hash.as_str(),
                "author": commit.author,
                "message": commit.message,
                "timestamp": commit.timestamp.to_rfc3339(),
                "files": [],
            }],
            // The canonical empty tree is a protocol sentinel, not a stored
            // tree object, so a complete response contains only the root.
            "trees": [oak_core::protocol::tree_to_wire(&root)],
        })))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_commits_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&commit.hash),
    )
    .await
    .expect("an omitted canonical empty child is still a complete tree closure");

    assert!(repo.get_commit(&commit.hash).unwrap().is_some());
    assert!(repo.get_tree(&root.hash).unwrap().is_some());
}

#[tokio::test]
async fn test_ensure_commits_local_rejects_same_cardinality_wrong_commit_without_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let manifest_hash = oak_core::Manifest::empty().hash;
    let requested = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        manifest_hash.clone(),
        "tester".to_string(),
        Some("requested".to_string()),
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let unrequested_manifest = oak_core::Manifest::new(vec![oak_core::ManifestEntry {
        path: "unrequested.txt".to_string(),
        blob_hash: hash_bytes(b"unrequested content"),
        mode: oak_core::FileMode::Regular,
    }]);
    let unrequested_trees = oak_core::build_tree(&unrequested_manifest.entries).unwrap();
    let unrequested = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        unrequested_manifest.hash.clone(),
        "tester".to_string(),
        Some("unrequested".to_string()),
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:01Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": unrequested.hash.as_str(),
                "branch_name": unrequested.branch_name,
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": manifest_hash.as_str(),
                "author": unrequested.author,
                "message": unrequested.message,
                "timestamp": unrequested.timestamp.to_rfc3339(),
                "files": [],
            }],
            "trees": unrequested_trees
                .trees
                .iter()
                .map(oak_core::protocol::tree_to_wire)
                .collect::<Vec<_>>(),
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_commits_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested.hash),
    )
    .await
    .expect_err("a same-cardinality wrong response must fail");

    assert!(error.to_string().contains("unrequested"), "got: {error}");
    assert!(repo.get_commit(&requested.hash).unwrap().is_none());
    assert!(
        repo.get_commit(&unrequested.hash).unwrap().is_none(),
        "response validation must happen before storing any remote object"
    );
    for tree in &unrequested_trees.trees {
        assert!(
            repo.get_tree(&tree.hash).unwrap().is_none(),
            "response validation must happen before storing remote trees"
        );
    }
}

#[tokio::test]
async fn test_ensure_commits_local_rejects_unreachable_extra_tree_without_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let requested_manifest = oak_core::Manifest::new(vec![oak_core::ManifestEntry {
        path: "requested.txt".to_string(),
        blob_hash: hash_bytes(b"requested content"),
        mode: oak_core::FileMode::Regular,
    }]);
    let requested_trees = oak_core::build_tree(&requested_manifest.entries).unwrap();
    let extra_manifest = oak_core::Manifest::new(vec![oak_core::ManifestEntry {
        path: "extra.txt".to_string(),
        blob_hash: hash_bytes(b"extra content"),
        mode: oak_core::FileMode::Regular,
    }]);
    let extra_trees = oak_core::build_tree(&extra_manifest.entries).unwrap();
    let requested = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        requested_manifest.hash.clone(),
        "tester".to_string(),
        Some("requested".to_string()),
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();
    let mut trees: Vec<_> = requested_trees
        .trees
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect();
    trees.extend(
        extra_trees
            .trees
            .iter()
            .map(oak_core::protocol::tree_to_wire),
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": requested.hash.as_str(),
                "branch_name": requested.branch_name,
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": requested.manifest_hash.as_str(),
                "author": requested.author,
                "message": requested.message,
                "timestamp": requested.timestamp.to_rfc3339(),
                "files": [],
            }],
            "trees": trees,
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_commits_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested.hash),
    )
    .await
    .expect_err("an unreachable extra tree must fail before persistence");

    assert!(
        error.to_string().contains("unreachable tree"),
        "got: {error}"
    );
    assert!(repo.get_commit(&requested.hash).unwrap().is_none());
    for tree in requested_trees.trees.iter().chain(&extra_trees.trees) {
        assert!(
            repo.get_tree(&tree.hash).unwrap().is_none(),
            "closure validation must happen before storing any tree"
        );
    }
}

#[tokio::test]
async fn test_ensure_commits_local_rejects_incomplete_tree_closure_without_writes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());
    let manifest = oak_core::Manifest::new(vec![oak_core::ManifestEntry {
        path: "nested/file.txt".to_string(),
        blob_hash: hash_bytes(b"nested content"),
        mode: oak_core::FileMode::Regular,
    }]);
    let built = oak_core::build_tree(&manifest.entries).unwrap();
    let root = built
        .trees
        .iter()
        .find(|tree| tree.hash == manifest.hash)
        .unwrap();
    let omitted = built
        .trees
        .iter()
        .find(|tree| tree.hash != manifest.hash)
        .unwrap();
    let requested = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("requested nested tree".to_string()),
        Vec::new(),
        chrono::DateTime::parse_from_rfc3339("2026-04-23T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": requested.hash.as_str(),
                "branch_name": requested.branch_name,
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": requested.manifest_hash.as_str(),
                "author": requested.author,
                "message": requested.message,
                "timestamp": requested.timestamp.to_rfc3339(),
                "files": [],
            }],
            "trees": [oak_core::protocol::tree_to_wire(root)],
        })))
        .mount(&server)
        .await;

    let error = oak_cli::commands::blob_fetch::ensure_commits_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&requested.hash),
    )
    .await
    .expect_err("an incomplete tree closure must fail before persistence");

    assert!(
        error.to_string().contains(omitted.hash.short()),
        "got: {error}"
    );
    assert!(repo.get_commit(&requested.hash).unwrap().is_none());
    assert!(
        repo.get_tree(&root.hash).unwrap().is_none(),
        "even a valid root must roll back when its child is absent"
    );
    assert!(repo.get_tree(&omitted.hash).unwrap().is_none());
}

/// The blob-store/chunk skew: the server's chunk refs were cut from the
/// zstd-compressed at-rest blob bytes, so the chunk hash-verifies (it IS an
/// honest chunk of the compressed frame) but reassembly yields the frame,
/// not the file. The client must detect this after reassembly, store the
/// decompressed plaintext, and fix the blob→chunk mapping — this is the bug
/// that materialized raw-zstd files into freshly cloned working trees.
#[tokio::test]
async fn test_ensure_blobs_local_repairs_compressed_blob_skew() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let plaintext = "the true file content\n".repeat(40).into_bytes();
    let blob_hash = hash_bytes(&plaintext);
    let frame = zstd::encode_all(&plaintext[..], 3).unwrap();
    let frame_hash = hash_bytes(&frame);
    assert_ne!(blob_hash, frame_hash, "test needs a real skew");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": blob_hash.as_str(),
                "size": frame.len(),
                "chunks": [{
                    "hash": frame_hash.as_str(),
                    "offset": 0,
                    "size": frame.len(),
                }],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": frame_hash.as_str(),
                "content": frame,
            }]
        })))
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .expect("the skewed blob is repaired, not stored corrupt");

    let stored = repo.get_blob(&blob_hash).unwrap().expect("blob stored");
    assert_eq!(stored.content, plaintext, "plaintext stored, not the frame");
    assert_eq!(stored.size, plaintext.len() as u64);

    // The mapping must describe the plaintext we stored — re-advertising the
    // frame's chunks on a later push/serve would re-poison the server.
    let chunks = repo
        .get_blob_chunks(&blob_hash)
        .unwrap()
        .expect("mapping replaced");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].hash, blob_hash, "self-chunk of the plaintext");
}

/// Bytes that match neither the blob hash nor decompress to it are
/// corruption: refuse to store rather than poisoning the local blob store
/// (which would flow straight into the working tree).
#[tokio::test]
async fn test_ensure_blobs_local_rejects_corrupt_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let claimed = hash_bytes(b"what the manifest expects");
    let garbage = b"entirely different bytes".to_vec();
    let garbage_hash = hash_bytes(&garbage);
    repo.set_metadata(oak_core::MetadataKey::KnownLostBlobs, claimed.as_str())
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": claimed.as_str(),
                "size": garbage.len(),
                "chunks": [{
                    "hash": garbage_hash.as_str(),
                    "offset": 0,
                    "size": garbage.len(),
                }],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": garbage_hash.as_str(),
                "content": garbage,
            }]
        })))
        .mount(&server)
        .await;

    let err = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&claimed),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("does not match its hash"),
        "got: {err}"
    );
    assert!(
        !repo.has_blob(&claimed).unwrap(),
        "corrupt content must not be stored under the claimed hash"
    );
    assert!(
        oak_cli::commands::known_loss::load_known_lost_blobs(&repo).contains(claimed.as_str()),
        "failed recovery must retain the exact known-loss marker"
    );
}

#[tokio::test]
async fn test_ensure_blobs_local_rejects_corrupt_chunk_without_caching_it() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let expected = b"expected chunk bytes".to_vec();
    let blob_hash = hash_bytes(&expected);
    let corrupt = b"different chunk bytes".to_vec();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": blob_hash.as_str(),
                "size": expected.len(),
                "chunks": [{
                    "hash": blob_hash.as_str(),
                    "offset": 0,
                    "size": expected.len(),
                }],
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/testowner/demo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": blob_hash.as_str(),
                "content": corrupt,
            }]
        })))
        .mount(&server)
        .await;

    let err = oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "testowner",
        "demo",
        None,
        std::slice::from_ref(&blob_hash),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("does not match content hash"),
        "got: {err}"
    );
    assert!(
        repo.get_chunk(&blob_hash).unwrap().is_none(),
        "corrupt chunk bytes must not poison the local cache"
    );
    assert!(
        !repo.has_blob(&blob_hash).unwrap(),
        "blob assembled from corrupt chunks must not be stored"
    );
}
