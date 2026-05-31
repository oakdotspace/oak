//! HTTP-level tests for `blob_fetch::ensure_blobs_local` using a mock server.
//!
//! Exercises the real reqwest client against wiremock-served responses so
//! the JSON shapes, concurrency, and error paths are verified end-to-end
//! without spinning up the full oak-server (which requires Postgres).

use std::path::Path;

use oak_core::{hash_bytes, Hash};
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
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": blob_hash.as_str(),
                "content": content,
            }]
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
    .expect("ensure_blobs_local should succeed");

    let stored = repo.get_blob(&blob_hash).unwrap().expect("blob stored");
    assert_eq!(stored.content, content);
    assert_eq!(stored.size, content.len() as u64);
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
async fn test_ensure_commits_local_fetches_missing_commit() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // Use an empty manifest to keep the commit payload self-contained —
    // SQLite FKs require referenced blobs to exist if we include entries.
    let manifest_hash = oak_core::Manifest::empty().hash;
    let commit_hash = Hash::from_hex(&"b".repeat(64)).unwrap();

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
