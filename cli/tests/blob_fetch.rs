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
