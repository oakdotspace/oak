use chrono::Utc;
use oak_cli::output;
use oak_core::{
    Branch, ChangeType, ChunkInfo, Commit, FileChange, FileMode, ManifestEntry, MetadataKey,
    OakError, Repository, SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn seed_one_commit(repo: &SqliteRepository, branch_name: &str) {
    seed_one_commit_with_content(repo, branch_name, b"hello world\n".to_vec());
}

fn seed_one_commit_with_content(
    repo: &SqliteRepository,
    branch_name: &str,
    content: Vec<u8>,
) -> oak_core::Hash {
    let branch = Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    );
    repo.store_branch(&branch).unwrap();
    repo.set_current_branch(branch_name).unwrap();

    let blob_hash = repo.put_blob(content).unwrap();
    let manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "hello.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let commit_hash = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "hello.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &commit_hash).unwrap();
    repo.set_head(&commit_hash).unwrap();
    blob_hash
}

fn seed_many_file_base_and_one_blob_delta(
    repo: &SqliteRepository,
    branch_name: &str,
) -> (oak_core::Hash, oak_core::Hash, oak_core::Hash) {
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch_name).unwrap();

    let mut entries = Vec::new();
    let mut base_changes = Vec::new();
    for index in 0..200 {
        let mut content = format!("base-file-{index:03}\n").into_bytes();
        content.resize(512, index as u8);
        let blob_hash = repo.put_blob(content).unwrap();
        let path = format!("file-{index:03}.txt");
        entries.push(ManifestEntry {
            path: path.clone(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        });
        base_changes.push(FileChange {
            path,
            change_type: ChangeType::Added,
            old_blob_hash: None,
            new_blob_hash: Some(blob_hash),
            old_path: None,
            old_mode: None,
            new_mode: Some(FileMode::Regular),
        });
    }
    let base_manifest = repo.put_manifest(entries.clone()).unwrap();
    let base = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            base_manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            base_changes,
        )
        .unwrap();

    let new_content = vec![0x5a; 512];
    let new_blob = repo.put_blob(new_content).unwrap();
    let new_path = "z-new-file.txt".to_string();
    entries.push(ManifestEntry {
        path: new_path.clone(),
        blob_hash: new_blob.clone(),
        mode: FileMode::Regular,
    });
    let tip_manifest = repo.put_manifest(entries).unwrap();
    let tip = repo
        .put_commit(
            branch_name.to_string(),
            Some(base.clone()),
            None,
            tip_manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: new_path,
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(new_blob.clone()),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &tip).unwrap();
    repo.set_head(&tip).unwrap();
    (base, tip, new_blob)
}

fn seed_existing_branch_delta_with_content(
    repo: &SqliteRepository,
    branch_name: &str,
    content: Vec<u8>,
) -> (oak_core::Hash, oak_core::Hash) {
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch_name).unwrap();
    let base = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    let blob_hash = repo.put_blob(content).unwrap();
    let manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "hello.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let tip = repo
        .put_commit(
            branch_name.to_string(),
            Some(base.clone()),
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "hello.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &tip).unwrap();
    repo.set_head(&tip).unwrap();
    (base, blob_hash)
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_invalid_local_commit_before_mutating_remote() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    // Reproduce a damaged local commit row: its content-address still names
    // the original author, but the persisted canonical fields no longer do.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute("UPDATE commits SET author = 'tampered'", [])
        .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("invalid local commit must fail before remote mutation");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase rejected outgoing commit"),
        "expected phase-specific diagnosis, got: {message}"
    );
    assert!(message.contains("commit hash mismatch"), "got: {message}");
    assert!(
        message.contains("no remote state was mutated"),
        "got: {message}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two head reads are allowed");
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET"),
        "admission failure must not create the repo, upload, or push: {requests:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_invalid_local_tree_before_mutating_remote() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let head = repo.get_branch_head(branch_name).unwrap().unwrap();
    let root_hash = repo.get_commit(&head).unwrap().unwrap().manifest_hash;

    // Keep the root's claimed address but replace its canonical bytes with a
    // different valid tree. A reader must detect the address/content split.
    let replacement = oak_core::Tree::new(Vec::new()).unwrap();
    let compressed = zstd::encode_all(replacement.canonical_bytes().as_slice(), 3).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE trees SET content = ?1 WHERE hash = ?2",
        rusqlite::params![compressed, root_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("invalid local tree must fail before remote mutation");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase rejected local tree"),
        "expected phase-specific diagnosis, got: {message}"
    );
    assert!(
        message.contains("does not match stored content hash"),
        "got: {message}"
    );
    assert!(
        message.contains("no remote state was mutated"),
        "got: {message}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two head reads are allowed");
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET"),
        "admission failure must not create the repo, upload, or push: {requests:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_corrupt_inline_blob_before_mutating_remote() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE blobs SET content = ?1, codec = 0 WHERE hash = ?2",
        rusqlite::params![b"jello world\n".to_vec(), blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("corrupt inline blob must fail before remote mutation");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase rejected local blob"),
        "got: {message}"
    );
    assert!(message.contains("hash mismatch"), "got: {message}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two head reads are allowed");
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_corrupt_large_blob_before_chunk_probe_or_upload() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let size = oak_core::LARGE_FILE_THRESHOLD as usize + 1;
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, vec![b'a'; size]);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE blobs SET content = ?1, codec = 0 WHERE hash = ?2",
        rusqlite::params![vec![b'b'; size], blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("corrupt large blob must fail before chunking network calls");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase rejected local blob"),
        "got: {message}"
    );
    assert!(message.contains("hash mismatch"), "got: {message}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two head reads are allowed");
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn new_repo_push_rejects_missing_local_blob_before_repo_creation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("a new remote cannot prove it owns the missing blob");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase is missing local blob"),
        "got: {message}"
    );
    assert!(message.contains(blob_hash.as_str()), "got: {message}");
    assert!(
        message.contains("remote repository does not exist"),
        "got: {message}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two head reads are allowed");
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn existing_repo_push_requires_remote_proof_for_missing_local_blob() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .and(body_json(serde_json::json!({
            "hashes": [blob_hash.as_str()]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": [blob_hash.as_str()]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("remote reports that the missing local blob is absent there too");

    let message = err.to_string();
    assert!(
        message.contains("push admission phase could not prove remote blob"),
        "got: {message}"
    );
    assert!(message.contains(blob_hash.as_str()), "got: {message}");
    let requests = server.received_requests().await.unwrap();
    let posts: Vec<_> = requests
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .collect();
    assert_eq!(
        posts.len(),
        1,
        "only the read-only blob proof POST is allowed"
    );
    assert_eq!(posts[0].url.path(), "/api/oak/oak/blobs/check");
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_sparse_one_file_push_does_not_spend_live_verification_quota() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-sparse-ordinary";
    let (base, tip, new_blob) = seed_many_file_base_and_one_blob_delta(&repo, branch_name);
    let base_commit = repo.get_commit(&base).unwrap().unwrap();
    let base_manifest = repo
        .get_manifest(&base_commit.manifest_hash)
        .unwrap()
        .unwrap();
    assert_eq!(base_manifest.entries.len(), 200);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for entry in &base_manifest.entries {
        conn.execute(
            "DELETE FROM blobs WHERE hash = ?1",
            rusqlite::params![entry.blob_hash.as_str()],
        )
        .unwrap();
    }
    drop(conn);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "legacy",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": false,
            "staged_abort_protocol": "v1",
            "known_loss_protocol": "report_v1",
            "ordinary_bootstrap_protocol": "headless_preload_v1",
            "content_receipt_enforcement_required": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    let live_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with({
            let live_calls = std::sync::Arc::clone(&live_calls);
            let new_blob = new_blob.to_string();
            move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if body["verify_content"].as_bool() == Some(true) {
                    let call = live_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if call > 6 {
                        return ResponseTemplate::new(429)
                            .set_body_string("live blob verification rate limit exceeded");
                    }
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "missing": [],
                        "verified_content": true
                    }));
                }
                let missing = body["hashes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|hash| hash.as_str() == Some(new_blob.as_str()))
                    .map(|_| new_blob.clone())
                    .collect::<Vec<_>>();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "missing": missing,
                    "verified_content": false
                }))
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": tip.as_str(),
            "message": "ordinary push accepted"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("a sparse one-file ordinary push must not spend generic live-proof quota");
    output::end_capture();

    assert_eq!(live_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let requests = server.received_requests().await.unwrap();
    let check_bodies: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/blobs/check")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert!(check_bodies
        .iter()
        .all(|body| body["verify_content"].as_bool() != Some(true)));
}

#[tokio::test(flavor = "current_thread")]
async fn existing_repo_push_hydrates_legacy_remote_blob_before_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let (base, blob_hash) =
        seed_existing_branch_delta_with_content(&repo, branch_name, b"hello world\n".to_vec());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": base.as_str()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .and(body_json(serde_json::json!({
            "hashes": [blob_hash.as_str()]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let content = b"hello world\n".to_vec();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch_name", branch_name))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "branch": { "name": branch_name },
            "blobs": [{
                "hash": blob_hash.as_str(),
                "content": [],
                "size": content.len(),
                "chunks": [{
                    "hash": blob_hash.as_str(),
                    "offset": 0,
                    "size": content.len()
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{
                "hash": blob_hash.as_str(),
                "content": content
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("legacy remote content must be hydrated and hash-verified before push");
    assert_eq!(repo.get_blob(&blob_hash).unwrap().unwrap().content, content);
}

#[tokio::test(flavor = "current_thread")]
async fn first_new_branch_on_v01021_branch_null_fails_actionably_before_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "DELETE FROM blobs WHERE hash = ?1",
            rusqlite::params![blob_hash.as_str()],
        )
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch_name", branch_name))
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
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("released server cannot hydrate content through an unpublished branch");
    let message = error.to_string();
    assert!(message.contains("returned branch:null"), "got: {message}");
    assert!(
        message.contains("deploy the fixed server first"),
        "got: {message}"
    );
    assert!(
        message.contains("no remote state was mutated"),
        "got: {message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn existing_repo_push_rejects_corrupt_legacy_hydration_before_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let (base, blob_hash) =
        seed_existing_branch_delta_with_content(&repo, branch_name, b"expected\n".to_vec());
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "DELETE FROM blobs WHERE hash = ?1",
            rusqlite::params![blob_hash.as_str()],
        )
        .unwrap();
    let corrupt = b"corrupt!\n".to_vec();
    let corrupt_hash = oak_core::hash_bytes(&corrupt);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch_name", branch_name))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "branch": { "name": branch_name },
            "blobs": [{
                "hash": blob_hash.as_str(),
                "content": [],
                "size": corrupt.len(),
                "chunks": [{
                    "hash": corrupt_hash.as_str(),
                    "offset": 0,
                    "size": corrupt.len()
                }]
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{"hash": corrupt_hash.as_str(), "content": corrupt}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("corrupt legacy hydration must fail before publication");
    output::end_capture();
    assert!(
        error.to_string().contains("does not match its hash"),
        "got: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn existing_repo_push_rejects_missing_legacy_pull_blob_before_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"expected\n".to_vec());
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute(
            "DELETE FROM blobs WHERE hash = ?1",
            rusqlite::params![blob_hash.as_str()],
        )
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch_name", branch_name))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "branch": { "name": branch_name },
            "blobs": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("missing legacy pull content must fail before publication");
    assert!(
        error.to_string().contains("omitted requested hash")
            && error.to_string().contains("no remote state was mutated"),
        "got: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn existing_repo_push_uses_strict_receipt_metadata_without_live_verification() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "staged_abort_protocol": "v1",
            "mapping_proof_protocol": "async_v1",
            "content_receipt_enforcement_required": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .and(body_json(serde_json::json!({
            "hashes": [blob_hash.as_str()],
            "require_verified_receipts": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": [],
            "verified_receipts_required": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("strict receipt metadata should make the outgoing commit complete");
    output::end_capture();
}

#[tokio::test(flavor = "current_thread")]
async fn staged_push_uses_receipt_ready_remote_blob_without_live_reverification() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());
    let mut parent = repo.get_branch_head(branch_name).unwrap().unwrap();
    let manifest = repo.get_commit(&parent).unwrap().unwrap().manifest_hash;
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    for index in 0..500 {
        let commit = Commit::with_timestamp(
            branch_name.to_string(),
            Some(parent),
            None,
            manifest.clone(),
            "tester".to_string(),
            Some(format!("staged {index}")),
            Vec::new(),
            time + chrono::Duration::seconds(index),
        )
        .unwrap();
        parent = commit.hash.clone();
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(branch_name, &parent).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with({
            let blob_hash = blob_hash.to_string();
            move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let missing = if body["require_verified_receipts"].as_bool() == Some(true) {
                    Vec::new()
                } else {
                    vec![blob_hash.clone()]
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "missing": missing,
                    "verified_content": false,
                    "verified_receipts_required": body["require_verified_receipts"].as_bool() == Some(true)
                }))
            }
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(4)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("receipt-ready remote bytes must close a staged outgoing operation");
    output::end_capture();
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| {
        request.url.path() != format!("/api/oak/oak/chunks/{}", blob_hash.as_str())
    }));
    let check: serde_json::Value = requests
        .iter()
        .find(|request| request.url.path() == "/api/oak/oak/blobs/check")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .unwrap();
    assert!(check.get("verify_content").is_none());
    assert_eq!(check["require_verified_receipts"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_blob_with_incorrect_stored_size_before_mutating_remote() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    let blob_hash = seed_one_commit_with_content(&repo, branch_name, b"hello world\n".to_vec());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE blobs SET size = size + 1 WHERE hash = ?1",
        rusqlite::params![blob_hash.as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("stored blob size mismatch must fail admission");
    assert!(
        err.to_string()
            .contains("stored size 13 does not match decoded content size 12"),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn feature_push_rejects_invalid_bootstrap_main_before_mutating_remote() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    seed_one_commit(&repo, "main");
    let feature = "tester-a71b16";
    repo.store_branch(&Branch::new(
        feature.to_string(),
        Some("Feature".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(feature).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE commits SET author = 'tampered' WHERE branch_name = 'main'",
        [],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(feature),
        false,
        None,
    )
    .await
    .expect_err("invalid bootstrap history must fail before remote mutation");

    assert!(
        err.to_string()
            .contains("push admission phase rejected outgoing commit"),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "read-only head discovery should run");
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET"),
        "invalid bootstrap history must not create the repo or upload: {requests:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_admits_corruption_after_first_500_before_any_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let mut parent = None;
    let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut last = None;
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            "main".to_string(),
            parent.clone(),
            None,
            oak_core::Tree::empty_hash(),
            "importer".to_string(),
            None,
            Vec::new(),
            base + chrono::Duration::seconds(index),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        parent = Some(commit.hash.clone());
        last = Some(commit.hash);
    }
    repo.set_branch_head("main", last.as_ref().unwrap())
        .unwrap();
    let feature = "tester-a71b16";
    repo.store_branch(&Branch::new(
        feature.to_string(),
        Some("Feature".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(feature).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE commits SET author = 'tampered' WHERE hash = ?1",
        rusqlite::params![last.unwrap().as_str()],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(feature),
        false,
        None,
    )
    .await
    .expect_err("later bootstrap corruption must fail before batch one");
    assert!(
        err.to_string()
            .contains("push admission phase rejected outgoing commit"),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_rejects_main_dependency_on_future_target_before_any_mutation() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let feature = "tester-a71b16";
    repo.store_branch(&Branch::new(
        feature.to_string(),
        Some("Feature".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let feature_commit = Commit::with_timestamp(
        feature.to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
    )
    .unwrap();
    repo.store_commit(&feature_commit).unwrap();
    repo.set_branch_head(feature, &feature_commit.hash).unwrap();

    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut parent = Some(feature_commit.hash);
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            "main".to_string(),
            parent,
            None,
            oak_core::Tree::empty_hash(),
            "importer".to_string(),
            None,
            Vec::new(),
            base + chrono::Duration::seconds(index),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        parent = Some(commit.hash);
    }
    repo.set_branch_head("main", parent.as_ref().unwrap())
        .unwrap();
    repo.set_current_branch(feature).unwrap();

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(feature),
        false,
        None,
    )
    .await
    .expect_err("main must not close over a not-yet-published target commit");
    assert!(
        err.to_string().contains("belongs to branch") && err.to_string().contains("tester-a71b16"),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn phase_one_push_sizes_publication_from_one_remote_missing_blob_not_full_manifest() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    let (base, tip, new_blob) = seed_many_file_base_and_one_blob_delta(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": false
        })))
        .expect(0)
        .mount(&server)
        .await;
    let missing_hash = new_blob.to_string();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(move |request: &wiremock::Request| {
            let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if request["verify_content"].as_bool() == Some(true) {
                return ResponseTemplate::new(429)
                    .set_body_string("phase-one live verification quota exceeded");
            }
            let requested = request["hashes"].as_array().unwrap();
            let missing = if requested
                .iter()
                .any(|hash| hash.as_str() == Some(missing_hash.as_str()))
            {
                vec![missing_hash.clone()]
            } else {
                Vec::new()
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": missing,
                "verified_content": false
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": tip.as_str(),
            "message": "ordinary phase-one push accepted"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("one actually missing blob fits ordinary publication during phase one");
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let push: serde_json::Value = requests
        .iter()
        .find(|request| request.url.path() == "/api/oak/oak/push")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .unwrap();
    assert_eq!(push["commits"].as_array().unwrap().len(), 1);
    assert_eq!(push["blobs"].as_array().unwrap().len(), 1);
    assert_eq!(push["blobs"][0]["hash"], new_blob.as_str());
    let checks: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/blobs/check")
        .collect();
    assert!(!checks.is_empty());
    let check_bodies: Vec<serde_json::Value> = checks
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert!(
        check_bodies
            .iter()
            .all(|request| !request["verify_content"].as_bool().unwrap_or(false)),
        "phase-one ordinary planning must not issue a physical proof: {check_bodies:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn forced_merge_dag_crossing_500_carries_force_through_every_staged_request() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let base_time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let base = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "importer".to_string(),
        None,
        Vec::new(),
        base_time,
    )
    .unwrap();
    repo.store_commit(&base).unwrap();
    let primary = Commit::with_timestamp(
        "main".to_string(),
        Some(base.hash.clone()),
        None,
        oak_core::Tree::empty_hash(),
        "importer".to_string(),
        None,
        Vec::new(),
        base_time + chrono::Duration::seconds(1),
    )
    .unwrap();
    repo.store_commit(&primary).unwrap();
    let mut side_parent = base.hash.clone();
    for index in 0..500 {
        let side = Commit::with_timestamp(
            "main".to_string(),
            Some(side_parent),
            None,
            oak_core::Tree::empty_hash(),
            "importer".to_string(),
            None,
            Vec::new(),
            base_time + chrono::Duration::seconds((2 + index) as i64),
        )
        .unwrap();
        repo.store_commit(&side).unwrap();
        side_parent = side.hash;
    }
    let merge_tip = Commit::with_timestamp(
        "main".to_string(),
        Some(primary.hash),
        Some(side_parent),
        oak_core::Tree::empty_hash(),
        "importer".to_string(),
        None,
        Vec::new(),
        base_time + chrono::Duration::seconds(600),
    )
    .unwrap();
    repo.store_commit(&merge_tip).unwrap();
    repo.set_branch_head("main", &merge_tip.hash).unwrap();

    let feature = "tester-a71b16";
    repo.store_branch(&Branch::new(
        feature.to_string(),
        Some("Feature".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(feature).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{feature}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/main"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(feature),
        true,
        None,
    )
    .await
    .expect("merge fan-in should stage without exposing a side-line head");
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let pushes: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/api/oak/oak/push/staged-v1"
        })
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(pushes.len(), 3);
    assert_eq!(pushes[0]["finalize"], false);
    assert_eq!(pushes[0]["expected_branch_head"], serde_json::Value::Null);
    assert_eq!(pushes[0]["commits"].as_array().unwrap().len(), 500);
    assert_eq!(pushes[1]["finalize"], false);
    assert_eq!(pushes[1]["commits"].as_array().unwrap().len(), 3);
    assert_eq!(pushes[2]["commits"].as_array().unwrap().len(), 0);
    assert_eq!(pushes[2]["target_head"], merge_tip.hash.as_str());
    assert_eq!(pushes[2]["finalize"], true);
    let stage_id = pushes[0]["stage_id"].as_str().unwrap();
    assert_eq!(stage_id.len(), 32);
    assert!(stage_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(pushes.iter().all(|request| request["stage_id"] == stage_id));
    assert!(
        pushes.iter().all(|request| request["force"] == true),
        "every object batch and finalizer must carry the same force decision"
    );

    // A server that cannot advertise the required staged publication must not
    // receive an oversized ordinary mutation. Small bridge operations retain
    // the ordinary path; this operation exceeds its atomic envelope.
    let mut legacy_tip = base.hash.clone();
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            feature.to_string(),
            Some(legacy_tip),
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            None,
            Vec::new(),
            base_time + chrono::Duration::seconds((1_000 + index) as i64),
        )
        .unwrap();
        legacy_tip = commit.hash.clone();
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(feature, &legacy_tip).unwrap();
    let unready = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": false
        })))
        .expect(1)
        .mount(&unready)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"head": base.hash.as_str()})),
        )
        .mount(&unready)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{feature}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"head": base.hash.as_str()})),
        )
        .mount(&unready)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "legacy ordinary push accepted"
        })))
        .expect(0)
        .mount(&unready)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&unready)
        .await;
    output::begin_capture();
    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &unready.uri(),
        "oak",
        "oak",
        Some(feature),
        true,
        None,
    )
    .await
    .expect_err("oversized publication requires staged_v1 before mutation");
    output::end_capture();
    assert!(
        error.to_string().contains("requires staged_v1")
            && error.to_string().contains("no remote state was mutated"),
        "got: {error}"
    );
    let legacy_requests = unready.received_requests().await.unwrap();
    assert!(legacy_requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));

    // Mixed deployment: a new replica could advertise staged_v1 while the
    // publication request lands on an old replica. The versioned path gets a
    // 404 and the client must fail, never retrying legacy `/push`.
    let mixed = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .mount(&mixed)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&mixed)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{feature}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mixed)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/main"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mixed)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mixed)
        .await;

    output::begin_capture();
    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &mixed.uri(),
        "oak",
        "oak",
        Some(feature),
        true,
        None,
    )
    .await
    .expect_err("old staged endpoint replica must fail without legacy fallback");
    output::end_capture();
    assert!(error.to_string().contains("staged-v1"), "got: {error}");
    let mixed_requests = mixed.received_requests().await.unwrap();
    assert!(mixed_requests
        .iter()
        .all(|request| request.url.path() != "/api/oak/oak/push"));

    // Drive the same 503-commit diamond through the real `oak serve` router,
    // storage, admission, staging, and final CAS. The wire assertions above
    // prove the 500+3+final request shape; this proves that shape is accepted
    // end to end and exposes only the intended merge tip.
    let serve_root = TempDir::new().unwrap();
    let serve_url = oak_cli::commands::serve::spawn_loopback(serve_root.path().to_path_buf())
        .await
        .unwrap();
    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &serve_url,
        "oak",
        "oak",
        Some(feature),
        false,
        None,
    )
    .await
    .expect("real oak serve must finalize the staged diamond");
    output::end_capture();
    let published: serde_json::Value = reqwest::Client::new()
        .get(format!("{serve_url}/api/oak/oak/branches/main"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["head"], merge_tip.hash.as_str());
}

#[tokio::test(flavor = "current_thread")]
async fn staged_push_pages_129_chunked_blobs_through_real_client_and_serve() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-proof-pages";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("proof page boundary".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let mut entries = Vec::new();
    for index in 0u32..129 {
        let bytes = index.to_be_bytes().to_vec();
        let hash = repo.put_blob(bytes.clone()).unwrap();
        repo.store_chunk(&hash, &bytes).unwrap();
        repo.store_blob_chunks(
            &hash,
            &[ChunkInfo {
                hash: hash.clone(),
                offset: 0,
                length: bytes.len() as u32,
            }],
        )
        .unwrap();
        entries.push(ManifestEntry {
            path: format!("objects/{index:03}.bin"),
            blob_hash: hash,
            mode: FileMode::Regular,
        });
    }
    let root = repo.put_manifest(entries).unwrap();
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut parent = None;
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            branch.to_string(),
            parent,
            None,
            root.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            time + chrono::Duration::seconds(index),
        )
        .unwrap();
        parent = Some(commit.hash.clone());
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(branch, parent.as_ref().unwrap())
        .unwrap();

    let serve_root = TempDir::new().unwrap();
    let serve_url = oak_cli::commands::serve::spawn_loopback(serve_root.path().to_path_buf())
        .await
        .unwrap();
    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &serve_url,
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect("129 chunked blobs must split into two proof sets before publication");
    output::end_capture();
}

#[tokio::test(flavor = "current_thread")]
async fn staged_push_uploads_receiptless_remote_blobs_without_generic_live_verification() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-hosted-proof-limit";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("hosted proof page".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let mut entries = Vec::new();
    for index in 0u32..33 {
        let mut bytes = vec![0; 4 * 1024];
        bytes[..4].copy_from_slice(&index.to_be_bytes());
        let hash = repo.put_blob(bytes).unwrap();
        entries.push(ManifestEntry {
            path: format!("objects/{index:03}.bin"),
            blob_hash: hash,
            mode: FileMode::Regular,
        });
    }
    let root = repo.put_manifest(entries).unwrap();
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut parent = None;
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            branch.to_string(),
            parent,
            None,
            root.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            time + chrono::Duration::seconds(index),
        )
        .unwrap();
        parent = Some(commit.hash.clone());
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(branch, parent.as_ref().unwrap())
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let hashes = body["hashes"].as_array().unwrap();
            let verify_content = body["verify_content"].as_bool().unwrap_or(false);
            if verify_content {
                ResponseTemplate::new(429)
                    .set_body_string("generic live verification is rate limited")
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "missing": hashes,
                    "verified_content": verify_content,
                    "verified_receipts_required": body["require_verified_receipts"].as_bool() == Some(true)
                }))
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/check"))
        .respond_with(ResponseTemplate::new(422).set_body_string("stop after proof paging"))
        .mount(&server)
        .await;

    output::begin_capture();
    let _ = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await;
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let check_bodies: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/blobs/check")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert!(check_bodies
        .iter()
        .any(|body| !body["verify_content"].as_bool().unwrap_or(false)));
    assert!(
        check_bodies
            .iter()
            .all(|body| body["verify_content"].as_bool() != Some(true)),
        "staged planning and upload selection must use metadata presence; exact staged publication performs receipt proof inside the staged session: {check_bodies:?}"
    );
    assert!(
        check_bodies
            .iter()
            .any(|body| body["require_verified_receipts"].as_bool() == Some(true)),
        "once staged_v1 is selected, upload selection must use finalization's strict receipt predicate: {check_bodies:?}"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/oak/oak/push/staged-v1")
            .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
            .any(|body| !body["blobs"].as_array().unwrap().is_empty()),
        "a mapping that exists but lacks strict receipts must enter the upload/proof path: {:?}",
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn staged_sparse_push_checks_more_than_192_locally_missing_blobs_without_live_quota() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch = "feature-sparse-staged";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("sparse staged".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let base = Commit::with_timestamp(
        branch.to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        time,
    )
    .unwrap();
    repo.store_commit(&base).unwrap();
    let mut entries = Vec::new();
    for index in 0u32..193 {
        let content = index.to_be_bytes().to_vec();
        let hash = repo.put_blob(content).unwrap();
        entries.push(ManifestEntry {
            path: format!("sparse/{index:03}.bin"),
            blob_hash: hash,
            mode: FileMode::Regular,
        });
    }
    let root = repo.put_manifest(entries).unwrap();
    let mut parent = Some(base.hash.clone());
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            branch.to_string(),
            parent,
            None,
            root.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            time + chrono::Duration::seconds(index + 1),
        )
        .unwrap();
        parent = Some(commit.hash.clone());
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(branch, parent.as_ref().unwrap())
        .unwrap();
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute("DELETE FROM blobs", [])
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": base.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": base.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["verify_content"].as_bool() == Some(true) {
                return ResponseTemplate::new(429).set_body_string("live quota exceeded");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": [],
                "verified_content": false,
                "verified_receipts_required": body["require_verified_receipts"].as_bool() == Some(true)
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect("receipt-ready staged publication must not spend generic live-proof quota");
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let checks: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/blobs/check")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert!(checks
        .iter()
        .all(|body| body["verify_content"].as_bool() != Some(true)));
    assert!(checks.iter().any(|body| {
        body["hashes"].as_array().unwrap().len() == 193
            && body["require_verified_receipts"].as_bool() == Some(true)
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn phase_one_large_first_push_preloads_objects_then_publishes_one_ordinary_head() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-phase-one-bootstrap";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("phase one bootstrap".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();

    let mut entries = Vec::new();
    for index in 0u32..129 {
        let mut bytes = vec![0; 4 * 1024];
        bytes[..4].copy_from_slice(&index.to_be_bytes());
        let hash = repo.put_blob(bytes).unwrap();
        entries.push(ManifestEntry {
            path: format!("objects/{index:03}.bin"),
            blob_hash: hash,
            mode: FileMode::Regular,
        });
    }
    let root = repo.put_manifest(entries).unwrap();
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut parent = None;
    for index in 0..501 {
        let commit = Commit::with_timestamp(
            branch.to_string(),
            parent,
            None,
            root.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            time + chrono::Duration::seconds(index),
        )
        .unwrap();
        parent = Some(commit.hash.clone());
        repo.store_commit(&commit).unwrap();
    }
    let tip = parent.unwrap();
    repo.set_branch_head(branch, &tip).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "legacy",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": false,
            "staged_abort_protocol": "v1",
            "known_loss_protocol": "report_v1",
            "ordinary_bootstrap_protocol": "headless_preload_v1",
            "mapping_proof_protocol": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": null})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body.get("verify_content").is_none());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": body["hashes"],
                "verified_content": false
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": tip.as_str(),
            "message": "ok"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect("a fixed phase-one server must accept a bounded large first publication");
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let ordinary: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/push")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert!(
        ordinary.len() >= 2,
        "objects must be preloaded before publication"
    );
    let final_request = ordinary.last().unwrap();
    assert_eq!(final_request["commits"].as_array().unwrap().len(), 501);
    assert!(final_request["blobs"].as_array().unwrap().is_empty());
    assert!(final_request["trees"].as_array().unwrap().is_empty());
    assert!(ordinary[..ordinary.len() - 1]
        .iter()
        .all(|request| request["commits"].as_array().unwrap().is_empty()));
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_mixed_chunk_check_page_before_upload_or_publication() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-mixed-chunk-check";
    let (base, _) = seed_existing_branch_delta_with_content(
        &repo,
        branch,
        vec![0x5a; oak_core::LARGE_FILE_THRESHOLD as usize],
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"head": base.as_str()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/check"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let requested = body["hashes"].as_array().unwrap();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": [
                    {"hash": requested[0].as_str().unwrap(), "upload_url": null},
                    {"hash": "cc".repeat(32), "upload_url": null},
                ]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    let error = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect_err("a mixed requested/unrequested check response must fail closed");
    output::end_capture();

    assert!(
        error.to_string().contains("unrequested hash"),
        "got: {error}"
    );
    let requests = server.received_requests().await.unwrap();
    let mutating: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.method.as_str() != "GET"
                && request.url.path() != "/api/oak/oak/chunks/check"
                && request.url.path() != "/api/oak/oak/blobs/check"
        })
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect();
    assert!(
        mutating.is_empty(),
        "unexpected mutation requests: {mutating:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_feature_push_uses_one_isolated_session_and_final_cas() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-large";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("large feature".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let boundary = Commit::with_timestamp(
        branch.to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        Some("published boundary".to_string()),
        Vec::new(),
        time,
    )
    .unwrap();
    repo.store_commit(&boundary).unwrap();
    let mut parent = boundary.hash.clone();
    for index in 0..1001 {
        let commit = Commit::with_timestamp(
            branch.to_string(),
            Some(parent),
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            Some(format!("feature {index}")),
            Vec::new(),
            time + chrono::Duration::seconds(index + 1),
        )
        .unwrap();
        parent = commit.hash.clone();
        repo.store_commit(&commit).unwrap();
    }
    repo.set_branch_head(branch, &parent).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": boundary.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "push_protocol": "staged_v1",
            "staged_session_protocol": "opaque_v1",
            "staged_capabilities_ready": true,
            "mapping_proof_protocol": "async_v1",
            "staged_abort_protocol": "v1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push/staged-v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect("oversized feature must stage then finalize");
    output::end_capture();

    let requests = server.received_requests().await.unwrap();
    let pushes: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/oak/oak/push/staged-v1")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(pushes.len(), 4);
    assert_eq!(pushes[0]["commits"].as_array().unwrap().len(), 500);
    assert_eq!(pushes[1]["commits"].as_array().unwrap().len(), 500);
    assert_eq!(pushes[2]["commits"].as_array().unwrap().len(), 1);
    assert_eq!(pushes[3]["finalize"], true);
    assert_eq!(pushes[3]["target_head"], parent.as_str());
    assert!(pushes
        .iter()
        .all(|request| request["expected_branch_head"] == boundary.hash.as_str()));
    assert!(pushes
        .iter()
        .all(|request| request["stage_id"] == pushes[0]["stage_id"]));

    // Exercise the same branch-general operation against the real local
    // server contract: seed only the proven boundary with an ordinary push,
    // then stage 1,001 descendants and atomically expose the planned tip.
    let serve_root = TempDir::new().unwrap();
    let serve_url = oak_cli::commands::serve::spawn_loopback(serve_root.path().to_path_buf())
        .await
        .unwrap();
    let client = reqwest::Client::new();
    client
        .post(format!("{serve_url}/api/repos"))
        .json(&serde_json::json!({
            "name": "oak",
            "organization_slug": "oak",
            "description": null,
            "is_public": false
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    client
        .post(format!("{serve_url}/api/oak/oak/push"))
        .json(&serde_json::json!({
            "expected_head": null,
            "expected_branch_head": null,
            "force": false,
            "branch": {
                "name": branch,
                "description": "large feature",
                "parent_branch": "main",
                "status": "open",
                "created_at": time.to_rfc3339(),
                "close_reason": null
            },
            "commits": [{
                "hash": boundary.hash.as_str(),
                "branch_name": branch,
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": boundary.manifest_hash.as_str(),
                "author": boundary.author,
                "message": boundary.message,
                "timestamp": boundary.timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &serve_url,
        "oak",
        "oak",
        Some(branch),
        false,
        None,
    )
    .await
    .expect("real oak serve must finalize a 1,001-commit feature session");
    output::end_capture();
    let published: serde_json::Value = client
        .get(format!("{serve_url}/api/oak/oak/branches/{branch}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["head"], parent.as_str());
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_valid_hash_commit_with_missing_parent_before_mutation() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Test".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let missing_parent = oak_core::Hash("ab".repeat(32));
    let commit = Commit::with_timestamp(
        branch_name.to_string(),
        Some(missing_parent.clone()),
        None,
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.set_branch_head(branch_name, &commit.hash).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(serde_json::json!({
            "hashes": [missing_parent.as_str()],
            "metadata_only": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("self-hashing orphan commit must fail graph admission");
    assert!(
        err.to_string()
            .contains("could not prove older remote commit edge")
            && err.to_string().contains(missing_parent.as_str()),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| {
        request.method.as_str() == "GET" || request.url.path() == "/api/oak/oak/commits/info"
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_valid_hash_commit_with_missing_merge_parent_before_mutation() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Test".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let missing_merge_parent = oak_core::Hash("ef".repeat(32));
    let commit = Commit::with_timestamp(
        branch_name.to_string(),
        None,
        Some(missing_merge_parent.clone()),
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.set_branch_head(branch_name, &commit.hash).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(serde_json::json!({
            "hashes": [missing_merge_parent.as_str()],
            "metadata_only": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("self-hashing commit with dangling merge edge must fail graph admission");
    assert!(
        err.to_string()
            .contains("could not prove older remote commit edge")
            && err.to_string().contains(missing_merge_parent.as_str()),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| {
        request.method.as_str() == "GET" || request.url.path() == "/api/oak/oak/commits/info"
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_first_push_proves_older_remote_merge_parent_without_full_tree_fetch() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let older = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "main".to_string(),
        None,
        Vec::new(),
        base,
    )
    .unwrap();
    repo.store_commit(&older).unwrap();
    let recent = Commit::with_timestamp(
        "main".to_string(),
        Some(older.hash.clone()),
        None,
        oak_core::Tree::empty_hash(),
        "main".to_string(),
        None,
        Vec::new(),
        base + chrono::Duration::seconds(1),
    )
    .unwrap();
    repo.store_commit(&recent).unwrap();
    repo.set_branch_head("main", &recent.hash).unwrap();

    let branch_name = "tester-a71b16";
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Delayed".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let feature = Commit::with_timestamp(
        branch_name.to_string(),
        Some(recent.hash.clone()),
        Some(older.hash.clone()),
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        base + chrono::Duration::seconds(2),
    )
    .unwrap();
    repo.store_commit(&feature).unwrap();
    repo.set_branch_head(branch_name, &feature.hash).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": recent.hash.as_str()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(serde_json::json!({
            "hashes": [older.hash.as_str()],
            "metadata_only": true
        })))
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
        .and(path("/api/oak/oak/commits/info"))
        .and(body_json(serde_json::json!({
            "hashes": [older.hash.as_str()],
            "metadata_only": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": older.hash.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "merge_parent_hash": null,
                "manifest_hash": older.manifest_hash.as_str(),
                "author": "main",
                "message": null,
                "timestamp": older.timestamp.to_rfc3339(),
                "files": []
            }],
            "trees": []
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": feature.hash.as_str(),
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("verified older main merge edge should not block a delayed first push");
    output::end_capture();
}

#[tokio::test(flavor = "current_thread")]
async fn push_fails_before_mutation_when_external_edge_proof_endpoint_is_unavailable() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let older = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "main".to_string(),
        None,
        Vec::new(),
        base,
    )
    .unwrap();
    repo.store_commit(&older).unwrap();
    let recent = Commit::with_timestamp(
        "main".to_string(),
        Some(older.hash.clone()),
        None,
        oak_core::Tree::empty_hash(),
        "main".to_string(),
        None,
        Vec::new(),
        base + chrono::Duration::seconds(1),
    )
    .unwrap();
    repo.store_commit(&recent).unwrap();
    repo.set_branch_head("main", &recent.hash).unwrap();

    let branch_name = "tester-a71b16";
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Delayed".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let feature = Commit::with_timestamp(
        branch_name.to_string(),
        Some(recent.hash.clone()),
        Some(older.hash.clone()),
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        base + chrono::Duration::seconds(2),
    )
    .unwrap();
    repo.store_commit(&feature).unwrap();
    repo.set_branch_head(branch_name, &feature.hash).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": recent.hash.as_str()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("a server without exact edge proofs must fail closed");
    assert!(
        err.to_string().contains("requires /commits/info")
            && err.to_string().contains("upgrade the server")
            && err.to_string().contains("no remote state was mutated"),
        "got: {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_rejects_dangling_commit_file_reference_before_mutation() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let dangling = "cd".repeat(32);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE commit_files SET new_blob_hash = ?1",
        rusqlite::params![dangling],
    )
    .unwrap();
    drop(conn);

    let server = MockServer::start().await;
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("v1-unhashed file references still require manifest closure");
    assert!(
        err.to_string().contains("file-change closure") && err.to_string().contains(&dangling),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn push_fails_closed_when_old_side_parent_manifest_is_not_local() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    repo.store_branch(&Branch::new(
        branch_name.to_string(),
        Some("Test".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let absent_parent = Commit::with_timestamp(
        branch_name.to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "tester".to_string(),
        None,
        Vec::new(),
        timestamp,
    )
    .unwrap();
    let blob = repo.put_blob(b"content\n".to_vec()).unwrap();
    let manifest = repo
        .put_manifest(vec![ManifestEntry {
            path: "new.txt".to_string(),
            blob_hash: blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let child = Commit::with_timestamp(
        branch_name.to_string(),
        Some(absent_parent.hash.clone()),
        None,
        manifest,
        "tester".to_string(),
        None,
        vec![FileChange {
            path: "new.txt".to_string(),
            change_type: ChangeType::Added,
            old_blob_hash: None,
            new_blob_hash: Some(blob),
            old_path: None,
            old_mode: None,
            new_mode: Some(FileMode::Regular),
        }],
        timestamp + chrono::Duration::seconds(1),
    )
    .unwrap();
    repo.store_commit(&child).unwrap();
    repo.set_branch_head(branch_name, &child.hash).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": absent_parent.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": absent_parent.hash.as_str()
        })))
        .mount(&server)
        .await;

    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("nonempty file metadata needs a locally materialized old side");
    assert!(
        err.to_string()
            .contains("cannot validate old-side references"),
        "got: {err}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .all(|request| request.method.as_str() == "GET"));
}

#[tokio::test(flavor = "current_thread")]
async fn push_repo_flag_links_and_creates_repo_non_interactively() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "hello agents\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch_name = repo.get_current_branch_name().unwrap().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos"))
        .and(body_json(serde_json::json!({
            "name": "widget",
            "description": null,
            "organization_slug": "agents"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widget"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::run(
        temp.path(),
        Some(&server.uri()),
        false,
        Some("agents/widget"),
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected push result, got: {captured:?}"
    );
    assert!(
        captured.contains("Run `oak merge` to land this on main"),
        "expected headless next-step guidance after creating a remote repo, got: {captured:?}"
    );

    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_metadata(MetadataKey::RepoOwner)
            .unwrap()
            .as_deref(),
        Some("agents")
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::RepoName).unwrap().as_deref(),
        Some("widget")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_json_reports_the_exact_published_head() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "hello agents\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap().to_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widget"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let root = temp.path().to_path_buf();
    let remote = server.uri();
    let command = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args([
                "push",
                "--remote",
                &remote,
                "--repo",
                "agents/widget",
                "--json",
            ])
            .current_dir(root)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env_remove("OAK_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        command.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&command.stdout),
        String::from_utf8_lossy(&command.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&command.stdout).unwrap();
    assert_eq!(output["schema_version"], 1);
    assert_eq!(output["pushed"], true);
    assert_eq!(output["published"], true);
    assert_eq!(output["repo"], "agents/widget");
    assert_eq!(output["branch"], branch);
    assert_eq!(output["pushed_head"], head);
    assert_eq!(output["current_branch_pushed_head"], head);
    assert_eq!(output["current_branch_push_checked"], true);

    let state = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    assert!(state.status.success());
    let state: serde_json::Value = serde_json::from_slice(&state.stdout).unwrap();
    assert_eq!(state["needs_push"], false);
    assert_eq!(state["current_branch_pushed_head"], head);
    assert_eq!(state["current_branch_push_source"], "local_push_receipt");

    repo.set_metadata(MetadataKey::RepoName, "different-widget")
        .unwrap();
    let relinked = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    let relinked: serde_json::Value = serde_json::from_slice(&relinked.stdout).unwrap();
    assert_eq!(relinked["needs_push"], true);
    assert!(relinked["current_branch_push_source"].is_null());
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();

    repo.set_metadata(MetadataKey::RepoOwner, "different-agents")
        .unwrap();
    let relinked_owner = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    let relinked_owner: serde_json::Value = serde_json::from_slice(&relinked_owner.stdout).unwrap();
    assert_eq!(relinked_owner["needs_push"], true);
    assert!(relinked_owner["current_branch_push_source"].is_null());
    repo.set_metadata(MetadataKey::RepoOwner, "agents").unwrap();

    std::fs::write(temp.path().join("hello.txt"), "new local work\n").unwrap();
    assert!(oak_cli::commands::commit::run(temp.path()).is_ok());
    let changed = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["needs_push"], true);
    assert_eq!(changed["current_branch_pushed_head"], head);

    std::fs::write(
        temp.path().join(".oak/LAST_PUSHED_HEAD.json"),
        serde_json::to_vec(&serde_json::json!({
            "remote": server.uri(),
            "branch": branch,
            "head": head,
        }))
        .unwrap(),
    )
    .unwrap();
    let legacy = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    let legacy: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
    assert_eq!(legacy["needs_push"], true);
    assert!(legacy["current_branch_push_source"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_json_distinguishes_an_exact_head_noop_from_publication() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "already published\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap().to_string();

    let server = MockServer::start().await;
    let credential_remote =
        server
            .uri()
            .replacen("http://", "http://QA_FAKE_USER:QA_FAKE_PASSWORD@", 1);
    let credential_remote =
        format!("{credential_remote}/?access_token=QA_QUERY_SECRET#QA_FRAGMENT_SECRET");
    repo.set_metadata(MetadataKey::RemoteUrl, &credential_remote)
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "agents").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();
    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": head.clone()
        })))
        .expect(1)
        .mount(&server)
        .await;

    let root = temp.path().to_path_buf();
    let command = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(["push", "--json"])
            .current_dir(root)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(command.status.success());
    let output: serde_json::Value = serde_json::from_slice(&command.stdout).unwrap();
    assert_eq!(output["pushed"], false);
    assert_eq!(output["published"], false);
    assert!(output.get("pushed_head").is_none());
    assert_eq!(output["current_branch_pushed_head"], head);
    let stdout = String::from_utf8_lossy(&command.stdout);
    let stderr = String::from_utf8_lossy(&command.stderr);
    let receipt = std::fs::read(temp.path().join(".oak/LAST_PUSHED_HEAD.json")).unwrap();
    let receipt = String::from_utf8_lossy(&receipt);
    for secret in [
        "QA_FAKE_USER",
        "QA_FAKE_PASSWORD",
        "QA_QUERY_SECRET",
        "QA_FRAGMENT_SECRET",
    ] {
        assert!(!stdout.contains(secret), "stdout leaked {secret}");
        assert!(!stderr.contains(secret), "stderr leaked {secret}");
        assert!(!receipt.contains(secret), "receipt leaked {secret}");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let root = temp.path().to_path_buf();
    let refreshed = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(["agent", "state", "--refresh", "--json"])
            .current_dir(root)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(refreshed.status.success());
    let refreshed: serde_json::Value = serde_json::from_slice(&refreshed.stdout).unwrap();
    assert_eq!(refreshed["needs_push"], true);

    let offline = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["agent", "state", "--json"])
        .current_dir(temp.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    let offline: serde_json::Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(offline["needs_push"], true);
    assert_eq!(
        offline["current_branch_push_source"],
        "remote_refresh_cache"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_unlinked_noninteractive_is_local_configuration_error() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "hello agents\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let server = MockServer::start().await;

    output::begin_capture();
    let err = oak_cli::commands::push::run(temp.path(), Some(&server.uri()), false, None)
        .await
        .unwrap_err();
    let captured = output::end_capture();

    let msg = err.to_string();
    assert!(
        matches!(err, OakError::Config(_)),
        "expected local configuration error, got: {msg}"
    );
    assert!(
        msg.contains("This repository isn't linked to a remote"),
        "error: {msg}"
    );
    assert!(msg.contains("oak push --repo <org>/<repo>"), "error: {msg}");
    assert!(
        !msg.contains("Server error"),
        "local setup error should not look like a server failure: {msg}"
    );
    assert!(
        captured.is_empty(),
        "unexpected captured output: {captured}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn bare_push_uses_stored_remote_for_linked_repo() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "agents").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();

    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .expect(1)
        .mount(&server)
        .await;

    let err = oak_cli::commands::push::run(temp.path(), None, false, None)
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("503"),
        "expected mock server failure, got: {err}"
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(server.uri()),
        "bare push must not rewrite a linked non-default remote"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_empty_remote_is_rejected_without_rewriting_stored_remote() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let stored = "https://stored.example";
    repo.set_metadata(MetadataKey::RemoteUrl, stored).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "agents").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();

    let err = oak_cli::commands::push::run(temp.path(), Some("  \t  "), false, None)
        .await
        .unwrap_err();

    assert!(
        matches!(err, OakError::InvalidArgument(_)),
        "expected invalid remote argument, got: {err}"
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl)
            .unwrap()
            .as_deref(),
        Some(stored),
        "invalid explicit remote must not corrupt stored metadata"
    );
}

#[test]
fn push_json_preflight_errors_emit_one_json_document() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec!["push", "--remote", "", "--json"],
        vec!["push", "--json"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(temp.path())
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_API_KEY")
            .env_remove("OAK_REMOTE")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("stdout was not JSON: {:?}", output.stdout));
        assert!(document["error"].is_object());
        assert_eq!(
            serde_json::Deserializer::from_slice(&output.stdout)
                .into_iter::<serde_json::Value>()
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_output_includes_branch_url() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    // A tiny all-inline payload skips the blobs/check round trip entirely —
    // the dedup probe costs more latency than just sending the bytes.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    // Piped/captured push prints a single result line — the step-by-step
    // narration (Pushing/Checking/Uploaded/Push complete) is terminal-only.
    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected single piped result line, got: {captured:?}"
    );
    assert!(!captured.contains("Pushing"), "got: {captured:?}");
    assert!(!captured.contains("Push complete"), "got: {captured:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn push_encodes_branch_name_in_api_path() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "feature/foo bar";
    seed_one_commit(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature%2Ffoo%20bar"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .unwrap();
    output::end_capture();
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_without_branch_keeps_plain_output() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Tiny all-inline payload: the blobs/check dedup round trip is skipped
    // (sending the bytes is cheaper than the probe).
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected single piped result line, got: {captured:?}"
    );
    assert!(!captured.contains("/branches/"));
}

#[tokio::test(flavor = "current_thread")]
async fn push_remote_commit_conflict_returns_one_actionable_error_without_preprinting() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    let remote_head = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Fetched twice: once by the push's concurrent head GET, once by the
    // self-heal probe re-resolving the branch head.
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": remote_head
        })))
        .expect(2)
        .mount(&server)
        .await;
    // No /commits/info on this server: the self-heal can't establish that
    // re-parenting is safe and must fall back to the actionable error.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("push should reject a remote branch head missing locally");
    let captured = output::end_capture();

    assert!(
        captured.is_empty(),
        "push_async should return the user-facing error and leave printing to the CLI entry point, got: {captured:?}"
    );
    assert!(
        matches!(err, OakError::RemoteCommitsNotInLocalHistory),
        "expected a specific push conflict variant, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "The remote branch has commits this clone doesn't have.\n\
         Run 'oak pull' to bring them in and converge (your local commits are kept), then push again."
    );
}

/// The trap, resolved by `oak push` itself (Invariant 3): the server's
/// branch head is a moved seed — a `main` commit minted when main advanced
/// under the branch, with no real work on the remote branch. Push must
/// re-parent automatically, print exactly one info line, and complete in
/// the same invocation.
#[tokio::test(flavor = "current_thread")]
async fn push_self_heals_when_remote_head_is_a_moved_seed() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    // The seed's snapshot on the server: one file the branch doesn't have.
    let theirs_content = b"merged on main\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = {
        let scratch_dir = TempDir::new().unwrap();
        let scratch = SqliteRepository::open(&scratch_dir.path().join("scratch.db")).unwrap();
        let root = scratch
            .put_manifest(vec![ManifestEntry {
                path: "main.txt".to_string(),
                blob_hash: theirs_blob.clone(),
                mode: FileMode::Regular,
            }])
            .unwrap();
        let mut fetch = |h: &oak_core::Hash| -> oak_core::Result<oak_core::Tree> {
            scratch
                .get_tree(h)?
                .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
        };
        let trees = oak_core::collect_tree_objects(&root, &mut fetch).unwrap();
        let wire: Vec<serde_json::Value> = trees
            .iter()
            .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
            .collect();
        (root, wire)
    };
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_001_000, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        root.clone(),
        "<remote>".to_string(),
        Some("merged something else".to_string()),
        Vec::new(),
        seed_timestamp,
    )
    .unwrap()
    .hash;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(&server)
        .await;
    // The seed is a `main` commit: a seed that moved, not foreign work.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": seed.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": root.as_str(),
                "author": "<remote>",
                "message": "merged something else",
                "timestamp": seed_timestamp.to_rfc3339(),
                "files": []
            }],
            "trees": wire_trees
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "blobs": [{
                "hash": theirs_blob.to_string(),
                "content": [],
                "size": theirs_content.len(),
                "chunks": [{
                    "hash": theirs_blob.to_string(),
                    "offset": 0,
                    "size": theirs_content.len()
                }]
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{
                "hash": theirs_blob.to_string(),
                "download_url": null,
                "content": theirs_content.to_vec()
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("push must self-heal a moved seed and complete");
    let captured = output::end_capture();

    // Exactly one re-parent info line, then the piped result line.
    let reparent_lines: Vec<&str> = captured
        .lines()
        .filter(|l| l.contains("re-parented onto main@"))
        .collect();
    assert_eq!(
        reparent_lines.len(),
        1,
        "expected exactly one re-parent line, got: {captured:?}"
    );
    assert!(
        reparent_lines[0].contains("main advanced since this branch was created"),
        "got: {captured:?}"
    );
    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "push must complete in the same invocation, got: {captured:?}"
    );

    // The branch now extends the seed; the old tip stays reachable.
    let new_head = repo.get_branch_head(branch_name).unwrap().unwrap();
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed.to_string())
    );
    assert_eq!(commit.merge_parent_hash, Some(old_tip.clone()));
    assert!(repo.get_commit(&old_tip).unwrap().is_some());

    // Overlay carries both sides.
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let mut paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["hello.txt", "main.txt"]);

    // Canonical main: the moved seed is recorded under the server's hash.
    assert_eq!(
        repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
        Some(seed.to_string())
    );
}

/// When the remote branch holds real foreign commits (its head is a commit
/// on the branch itself, not a moved seed), push must NOT touch anything —
/// one instruction: `oak pull`.
#[tokio::test(flavor = "current_thread")]
async fn push_does_not_self_heal_over_foreign_branch_commits() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    let foreign: String = "b2".repeat(32);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": foreign
        })))
        .mount(&server)
        .await;
    // The remote head is a commit on the branch itself — real foreign work.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": foreign,
                "branch_name": branch_name,
                "parent_hash": null,
                "manifest_hash": oak_core::Tree::empty_hash().to_string(),
                "author": "someone-else",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "files": []
            }],
            "trees": []
        })))
        .mount(&server)
        .await;

    output::begin_capture();
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("foreign remote commits must abort the push");
    output::end_capture();

    assert!(
        matches!(err, OakError::RemoteCommitsNotInLocalHistory),
        "expected the pull instruction, got: {err:?}"
    );
    // Nothing moved locally.
    assert_eq!(repo.get_branch_head(branch_name).unwrap(), Some(old_tip));
}

/// The deferred-heal retry: a prior attempt (e.g. the auto-push inside
/// `oak commit`, which holds the workdir lock) already ingested the moved
/// seed locally, so the seed IS in the local DB — but the branch still
/// doesn't extend it. The next push must detect divergence by ANCESTRY and
/// self-heal, not wave the stale chain through to a server-side rejection.
#[tokio::test(flavor = "current_thread")]
async fn push_self_heals_when_seed_is_already_ingested_but_not_ancestor() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    // The moved seed, already fully materialized locally (commit row +
    // empty tree) — exactly what a deferred self-heal leaves behind.
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_001_100, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        oak_core::Tree::empty_hash(),
        "<remote>".to_string(),
        None,
        Vec::new(),
        seed_timestamp,
    )
    .unwrap();
    repo.store_commit(&seed).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.hash.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("retry after a deferred heal must self-heal and push");
    let captured = output::end_capture();

    assert!(
        captured.contains("re-parented onto main@"),
        "expected the re-parent line, got: {captured:?}"
    );
    let new_head = repo.get_branch_head(branch_name).unwrap().unwrap();
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed.hash.to_string())
    );
    assert_eq!(commit.merge_parent_hash, Some(old_tip));
}
