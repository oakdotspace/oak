//! Zero-byte files, end to end (fb-193, fb-194).
//!
//! The empty blob is the one object whose bytes are implied by its hash, and
//! that turned out to matter: a server whose blob migration filtered on
//! `octet_length(content) > 0` keeps a metadata-only row for it — `blobs/check`
//! reports it present while pull omits it — so repos containing an empty file
//! could neither be cloned nor repaired by re-pushing.
//!
//! Reconstructing the empty blob is not one check but seven, because the
//! "every blob is present" question is asked from seven places, each with its
//! own error. Every one of them has a test here that fails without its
//! reconstruction, and each names the operation a user actually ran:
//!
//! - `oak status` / `oak commit` over a HEAD whose empty blob row is gone
//!   (fb-194 exactly) — the snapshot read in `merge::missing_blobs_in_manifest`
//! - `oak restore` of a deleted zero-byte file — `materialize::apply_manifest`
//! - `oak clone` from a server with no row for the empty blob —
//!   `repo::write_working_directory`
//! - `oak pull` of the same — `pull::update_working_dir`'s own preflight
//! - `oak clone` from a server whose metadata-only row makes pull advertise
//!   the blob with no chunk refs — the shared `pull::fetch_and_store_blobs`
//! - a mount/pinned-dep blob fetch for the empty hash, and one whose
//!   `blobs/info` reply names it chunkless — `blob_fetch::ensure_blobs_local`
//! - `oak push` after a `blobs/check` that claims the server has it
//!
//! The happy round trip through the in-repo reference server (`oak serve`)
//! and the contract's other half — a blob that genuinely *can't* be derived
//! must still fail the clone — bracket the set.

use std::fs;
use std::path::Path;

use oak_core::{
    Blob, Branch, ChunkInfo, Commit, FileMode, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Materialize a repo with two zero-byte files in different directories (so
/// the single shared empty blob is exercised through dedup) plus one ordinary
/// file, and commit it.
fn seed_source_repo(path: &Path) {
    oak_cli::commands::init::run(path, false).unwrap();
    fs::create_dir_all(path.join("docs")).unwrap();
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("docs/PLACEHOLDER"), b"").unwrap();
    fs::write(path.join("src/PLACEHOLDER"), b"").unwrap();
    fs::write(path.join("README.md"), b"hello agents\n").unwrap();
    oak_cli::commands::commit::run(path).unwrap();
}

fn assert_zero_byte_file(path: &Path) {
    let meta = fs::metadata(path)
        .unwrap_or_else(|e| panic!("{} should exist after clone: {e}", path.display()));
    assert_eq!(meta.len(), 0, "{} should be zero bytes", path.display());
}

/// Put a real, fully-committed repo into fb-194's state: HEAD's manifest
/// still names the empty blob, but the local `blobs` row for it is gone.
///
/// That is the shape a partial clone leaves behind — the manifest, trees, and
/// ordinary blobs all arrived, the zero-byte files are sitting on disk with
/// live stat-cache rows, and the one object the server never shipped is
/// missing. Everything except the deletion goes through the ordinary commit
/// path, so the stat cache is genuine: the next scan trusts its rows and does
/// *not* re-store the empty blob on the way past, which is exactly why the
/// reporter's `oak status` and `oak commit` both failed while the files were
/// visibly present.
///
/// `Repository` deliberately has no blob delete, so the row goes out through
/// SQLite directly.
fn seed_repo_whose_empty_blob_row_is_gone(path: &Path) {
    seed_source_repo(path);
    let db = path.join(".oak/oak.db");
    let empty_hash = Blob::empty_hash();

    let repo = SqliteRepository::open(&db).unwrap();
    assert!(
        repo.has_blob(&empty_hash).unwrap(),
        "a normal commit of zero-byte files must store the empty blob — \
         otherwise this fixture proves nothing"
    );
    drop(repo);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let deleted = conn
        .execute(
            "DELETE FROM blobs WHERE hash = ?1",
            rusqlite::params![empty_hash.as_str()],
        )
        .unwrap();
    assert_eq!(deleted, 1, "the empty blob row must have been removed");
    drop(conn);

    let repo = SqliteRepository::open(&db).unwrap();
    assert!(
        !repo.has_blob(&empty_hash).unwrap(),
        "the local store must genuinely lack the empty blob HEAD references"
    );
    assert!(
        repo.get_manifest(
            &repo
                .get_commit(&repo.get_head().unwrap().unwrap())
                .unwrap()
                .unwrap()
                .manifest_hash
        )
        .unwrap()
        .unwrap()
        .entries
        .iter()
        .any(|e| e.blob_hash == empty_hash),
        "HEAD's manifest must still reference the now-absent empty blob"
    );
}

/// fb-194 as reported: `oak status` in a repo whose HEAD manifest names the
/// empty blob the local store doesn't have. Resolving HEAD's snapshot happens
/// *before* the working-tree scan, so nothing gets a chance to re-store the
/// blob first — the read either reconstructs it or reports the snapshot torn.
#[test]
fn status_survives_a_head_whose_empty_blob_the_local_store_lacks() {
    let dir = TempDir::new().unwrap();
    seed_repo_whose_empty_blob_row_is_gone(dir.path());

    oak_cli::commands::status::run(dir.path(), false)
        .expect("`oak status` must not report a torn snapshot over a derivable blob");

    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert!(
        repo.has_blob(&Blob::empty_hash()).unwrap(),
        "reading the snapshot must leave the empty blob repaired in the local store"
    );
}

/// The operation fb-194's reporter could not perform. `oak commit` scans the
/// working tree first, but the scan hits its stat cache for the untouched
/// zero-byte files and stores nothing — so HEAD resolution still meets a
/// manifest whose empty blob is absent, and the commit died there even though
/// the files were on disk.
#[test]
fn commit_succeeds_when_head_references_an_empty_blob_the_local_store_lacks() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo_whose_empty_blob_row_is_gone(root);

    fs::write(root.join("NOTES.md"), b"follow-up\n").unwrap();
    oak_cli::commands::commit::run(root)
        .expect("a follow-up commit over a manifest missing only the empty blob must succeed");

    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    assert!(
        repo.has_blob(&Blob::empty_hash()).unwrap(),
        "the empty blob must be back in the local store after the commit"
    );

    // The zero-byte files survived the round trip: still on disk, still
    // carried by the new HEAD rather than dropped as unreadable.
    assert_zero_byte_file(&root.join("docs/PLACEHOLDER"));
    assert_zero_byte_file(&root.join("src/PLACEHOLDER"));
    let head = repo
        .get_commit(&repo.get_head().unwrap().unwrap())
        .unwrap()
        .unwrap();
    let manifest = repo.get_manifest(&head.manifest_hash).unwrap().unwrap();
    let mut paths: Vec<&str> = manifest
        .entries
        .iter()
        .filter(|e| e.blob_hash == Blob::empty_hash())
        .map(|e| e.path.as_str())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["docs/PLACEHOLDER", "src/PLACEHOLDER"]);
    assert!(manifest.entries.iter().any(|e| e.path == "NOTES.md"));
}

/// Push a repo containing zero-byte files to `oak serve`, clone it back, and
/// commit again in the clone. Before the empty blob was made reconstructible
/// this was three separate failures: the clone refused to write a partial
/// working tree, `blobs/check` claimed the blob was present so re-pushing
/// couldn't repair it, and the snapshot read behind `oak commit` reported
/// `incomplete blob data`.
#[tokio::test(flavor = "multi_thread")]
async fn zero_byte_files_round_trip_through_serve() {
    let src = TempDir::new().unwrap();
    seed_source_repo(src.path());

    let data = TempDir::new().unwrap();
    let base = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();

    oak_cli::commands::push::run(src.path(), Some(&base), false, Some("acme/widget"))
        .await
        .expect("pushing a repo with zero-byte files must succeed");

    let dest = TempDir::new().unwrap();
    let clone_dir = dest.path().join("widget");
    oak_cli::commands::repo::clone_repo(&base, "acme/widget", &clone_dir, false)
        .await
        .expect("cloning a repo with zero-byte files must succeed");

    assert_zero_byte_file(&clone_dir.join("docs/PLACEHOLDER"));
    assert_zero_byte_file(&clone_dir.join("src/PLACEHOLDER"));
    assert_eq!(
        fs::read(clone_dir.join("README.md")).unwrap(),
        b"hello agents\n"
    );

    // fb-194: the reporter could recreate the empty file by hand but still
    // couldn't commit, because resolving the snapshot manifest tripped over
    // the blob the server never shipped.
    fs::write(clone_dir.join("NOTES.md"), b"follow-up\n").unwrap();
    oak_cli::commands::commit::run(&clone_dir)
        .expect("a follow-up commit in a clone holding zero-byte files must succeed");
}

/// How the empty blob is represented in a server-side store. Both shapes are
/// real post-migration states and they reach the client through different
/// code: `Absent` makes pull omit the blob entirely, `MetadataOnly` makes pull
/// advertise it with an empty `chunks` list.
#[derive(Clone, Copy, PartialEq)]
enum EmptyBlobOnServer {
    /// No `blobs` row at all.
    Absent,
    /// A zero-length row with no chunks and no object-storage backing — what a
    /// blob migration that filtered on `octet_length(content) > 0` leaves.
    /// `blobs/check` reports it present; pull ships it chunkless.
    MetadataOnly,
}

/// Seed a server-side store (the layout `oak serve` reads) with a one-commit
/// repo holding `README.md` plus a zero-byte `docs/PLACEHOLDER`, and control
/// exactly how the empty blob is represented. `data_root` is the directory
/// handed to `spawn_loopback`.
fn seed_server_repo(data_root: &Path, empty_blob: EmptyBlobOnServer) {
    let owner_dir = data_root.join("acme");
    fs::create_dir_all(&owner_dir).unwrap();
    let srv = SqliteRepository::open_relaxed(&owner_dir.join("widget.oakdb")).unwrap();

    // An ordinary blob, stored the way a real push leaves it: content plus a
    // single self-chunk mapping, which is what pull re-advertises.
    let readme = b"hello agents\n".to_vec();
    let readme_hash = srv.put_blob(readme.clone()).unwrap();
    srv.store_chunk(&readme_hash, &readme).unwrap();
    srv.store_blob_chunks(
        &readme_hash,
        &[ChunkInfo {
            hash: readme_hash.clone(),
            offset: 0,
            length: readme.len() as u32,
        }],
    )
    .unwrap();

    let empty_hash = Blob::empty_hash();
    if empty_blob == EmptyBlobOnServer::MetadataOnly {
        // Content row only — deliberately no `store_chunk` / `store_blob_chunks`.
        srv.store_blob(&Blob::empty()).unwrap();
    }
    assert_eq!(
        srv.has_blob(&empty_hash).unwrap(),
        empty_blob == EmptyBlobOnServer::MetadataOnly,
        "the server-side shape of the empty blob is what these regressions turn on"
    );
    assert!(
        srv.get_blob_chunks(&empty_hash)
            .unwrap()
            .is_none_or(|c| c.is_empty()),
        "the empty blob must never have chunks server-side"
    );

    let manifest_hash = srv
        .put_manifest(vec![
            ManifestEntry {
                path: "README.md".to_string(),
                blob_hash: readme_hash,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "docs/PLACEHOLDER".to_string(),
                blob_hash: empty_hash,
                mode: FileMode::Regular,
            },
        ])
        .unwrap();

    srv.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let commit = Commit::new(
        "main".to_string(),
        None,
        None,
        manifest_hash,
        "tester".to_string(),
        None,
        Vec::new(),
    )
    .unwrap();
    srv.store_commit(&commit).unwrap();
    srv.set_branch_head("main", &commit.hash).unwrap();
}

/// The fb-193 server state, reproduced exactly: the repo's commit, trees, and
/// ordinary blobs are all present, but the empty blob has no row at all, so
/// the pull response omits it entirely. The client must derive it from its
/// hash and write the file — while still refusing any blob it *can't* derive.
#[tokio::test(flavor = "multi_thread")]
async fn clone_synthesizes_the_empty_blob_when_the_server_omits_it() {
    let data = TempDir::new().unwrap();
    seed_server_repo(&data.path().join("data"), EmptyBlobOnServer::Absent);
    let empty_hash = Blob::empty_hash();

    let base = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();

    let dest = TempDir::new().unwrap();
    let clone_dir = dest.path().join("widget");
    oak_cli::commands::repo::clone_repo(&base, "acme/widget", &clone_dir, false)
        .await
        .expect("a clone must survive a server that omits the empty blob");

    assert_zero_byte_file(&clone_dir.join("docs/PLACEHOLDER"));
    assert_eq!(
        fs::read(clone_dir.join("README.md")).unwrap(),
        b"hello agents\n"
    );

    // The synthesized blob is a real local object, not just a file on disk —
    // so status/commit/merge see a complete snapshot from here on.
    let cloned = SqliteRepository::open(&clone_dir.join(".oak/oak.db")).unwrap();
    assert!(cloned.has_blob(&empty_hash).unwrap());
    assert_eq!(
        cloned
            .get_metadata(MetadataKey::RepoName)
            .unwrap()
            .as_deref(),
        Some("widget")
    );

    fs::write(clone_dir.join("NOTES.md"), b"follow-up\n").unwrap();
    oak_cli::commands::commit::run(&clone_dir)
        .expect("committing after a synthesized empty blob must succeed");
}

/// The other post-migration shape: the server *does* have a `blobs` row for
/// the empty blob, it just has no chunks behind it — so pull advertises the
/// blob with an empty `chunks` list. The shared pull/clone blob ingest reads
/// "no chunk refs" as unreachable bytes and refuses the whole transfer, which
/// is right for every hash except this one.
#[tokio::test(flavor = "multi_thread")]
async fn clone_accepts_the_chunkless_empty_blob_a_metadata_only_row_advertises() {
    let data = TempDir::new().unwrap();
    seed_server_repo(&data.path().join("data"), EmptyBlobOnServer::MetadataOnly);

    let base = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();

    let dest = TempDir::new().unwrap();
    let clone_dir = dest.path().join("widget");
    oak_cli::commands::repo::clone_repo(&base, "acme/widget", &clone_dir, false)
        .await
        .expect("a chunkless empty blob in the pull response must not fail the transfer");

    assert_zero_byte_file(&clone_dir.join("docs/PLACEHOLDER"));
    let cloned = SqliteRepository::open(&clone_dir.join(".oak/oak.db")).unwrap();
    assert!(cloned.has_blob(&Blob::empty_hash()).unwrap());
}

/// `oak pull` reaches the working tree through its own missing-blob preflight,
/// not `apply_manifest`'s. A repo pulled from a server that never shipped the
/// empty blob would otherwise be reported as a pull that "is missing blob
/// af13… for 'docs/PLACEHOLDER'" and refuse to write anything.
#[tokio::test(flavor = "multi_thread")]
async fn pull_writes_a_zero_byte_file_the_server_never_shipped() {
    let data = TempDir::new().unwrap();
    seed_server_repo(&data.path().join("data"), EmptyBlobOnServer::Absent);

    let base = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();

    // A local repo sitting on `main` with no commits of its own — the first
    // pull is a pure fast-forward onto the server's head.
    let local = TempDir::new().unwrap();
    let oak_dir = local.path().join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.set_current_branch("main").unwrap();
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&oak_dir).unwrap();

    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &base,
        "acme/widget/pull",
        Some("main"),
        None,
        false,
        local.path(),
        None,
    )
    .await
    .expect("a pull whose manifest names an unshipped empty blob must still materialize");

    assert_zero_byte_file(&local.path().join("docs/PLACEHOLDER"));
    assert_eq!(
        fs::read(local.path().join("README.md")).unwrap(),
        b"hello agents\n"
    );
    assert!(repo.has_blob(&Blob::empty_hash()).unwrap());
}

/// `oak restore` rebuilds files straight from HEAD's manifest via the shared
/// materializer, without any snapshot read in front of it to repair the store.
/// A zero-byte file deleted from a working tree whose empty blob row is gone
/// must still come back rather than aborting the restore as a torn snapshot.
#[test]
fn restore_rebuilds_a_zero_byte_file_whose_empty_blob_row_is_gone() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    seed_repo_whose_empty_blob_row_is_gone(root);

    fs::remove_file(root.join("docs/PLACEHOLDER")).unwrap();
    fs::remove_file(root.join("src/PLACEHOLDER")).unwrap();

    oak_cli::commands::restore::run(root, &[], None, true)
        .expect("restoring a zero-byte file must not require the blob the server dropped");

    assert_zero_byte_file(&root.join("docs/PLACEHOLDER"));
    assert_zero_byte_file(&root.join("src/PLACEHOLDER"));
    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    assert!(repo.has_blob(&Blob::empty_hash()).unwrap());
}

/// `blobs/check` is a bandwidth optimization, and a server that wrongly
/// reports the empty blob present turns it into a trap: the one push that
/// would repair the server drops the very blob it is missing. The empty blob
/// carries zero content bytes, so push keeps it regardless of the answer.
#[tokio::test(flavor = "multi_thread")]
async fn push_re_sends_the_empty_blob_even_when_the_server_claims_to_have_it() {
    let src = TempDir::new().unwrap();
    oak_cli::commands::init::run(src.path(), false).unwrap();
    fs::create_dir_all(src.path().join("docs")).unwrap();
    fs::write(src.path().join("docs/PLACEHOLDER"), b"").unwrap();
    // Big enough that push actually issues the `blobs/check` round trip
    // rather than taking its small-payload shortcut.
    fs::write(src.path().join("BIG.md"), vec![b'x'; 128 * 1024]).unwrap();
    oak_cli::commands::commit::run(src.path()).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/widget"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widget", "head": null, "is_public": true
        })))
        .mount(&server)
        .await;
    // The fb-193 answer: the server insists it already has everything.
    Mock::given(method("POST"))
        .and(path("/api/acme/widget/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/acme/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "new_head": null, "message": "ok"
        })))
        .mount(&server)
        .await;

    oak_cli::commands::push::run(src.path(), Some(&server.uri()), false, Some("acme/widget"))
        .await
        .unwrap();

    let pushed = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/api/acme/widget/push")
        .expect("push request must have been sent");
    let body: serde_json::Value = serde_json::from_slice(&pushed.body).unwrap();
    let hashes: Vec<&str> = body["blobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["hash"].as_str().unwrap())
        .collect();
    assert_eq!(
        hashes,
        vec![Blob::empty_hash().as_str()],
        "the empty blob must survive `blobs/check` dedup; everything else may be dropped"
    );
}

/// The other half of the contract: only the empty blob is derivable. A
/// manifest entry whose blob is genuinely missing must still fail the clone
/// rather than quietly producing a partial working tree.
#[tokio::test(flavor = "multi_thread")]
async fn clone_still_refuses_a_blob_it_cannot_derive() {
    let data = TempDir::new().unwrap();
    let owner_dir = data.path().join("data").join("acme");
    fs::create_dir_all(&owner_dir).unwrap();
    let srv = SqliteRepository::open_relaxed(&owner_dir.join("widget.oakdb")).unwrap();

    // A manifest entry pointing at content the server never had, and whose
    // bytes nothing can reconstruct.
    let ghost = Blob::new(b"content only the pusher ever saw\n".to_vec()).hash;
    let manifest_hash = srv
        .put_manifest(vec![ManifestEntry {
            path: "ghost.txt".to_string(),
            blob_hash: ghost,
            mode: FileMode::Regular,
        }])
        .unwrap();
    srv.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    let commit = Commit::new(
        "main".to_string(),
        None,
        None,
        manifest_hash,
        "tester".to_string(),
        None,
        Vec::new(),
    )
    .unwrap();
    srv.store_commit(&commit).unwrap();
    srv.set_branch_head("main", &commit.hash).unwrap();
    drop(srv);

    let base = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();

    let dest = TempDir::new().unwrap();
    let err =
        oak_cli::commands::repo::clone_repo(&base, "acme/widget", &dest.path().join("w"), false)
            .await
            .expect_err("an underivable missing blob must still fail the clone");
    let msg = err.to_string();
    assert!(
        msg.contains("missing blob") && msg.contains("ghost.txt"),
        "expected the partial-working-tree refusal, got: {msg}"
    );
}

/// `oak mount` and the pinned-dependency reader fetch individual blobs by
/// hash. Asking the network for the empty blob is both wasteful and, on a
/// server holding only a metadata-only row, fatal — the fetch would fail for
/// every blob in the batch. It never leaves the machine: no request is issued
/// at all, and the blob is reconstructed locally.
#[tokio::test]
async fn ensure_blobs_local_reconstructs_the_empty_blob_without_touching_the_network() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

    let empty_hash = Blob::empty_hash();
    assert!(!repo.has_blob(&empty_hash).unwrap(), "precondition");

    // A server with no routes mounted: every request 404s, so any round trip
    // at all turns into a hard failure here.
    let server = MockServer::start().await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "acme",
        "widget",
        None,
        std::slice::from_ref(&empty_hash),
    )
    .await
    .expect("the empty blob must be satisfied locally, never fetched");

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "fetching only the empty blob must issue no HTTP requests"
    );
    let stored = repo.get_blob(&empty_hash).unwrap().expect("blob stored");
    assert!(stored.content.is_empty());
    assert_eq!(stored.size, 0);
}

/// The same "no chunk refs means unreachable bytes" rule guards the
/// `blobs/info` reply, and a server can name the empty blob there alongside
/// the blob that was asked for. One chunkless empty blob must not condemn the
/// whole batch — the ordinary blob still has to land.
#[tokio::test]
async fn ensure_blobs_local_survives_a_chunkless_empty_blob_in_a_blobs_info_reply() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();

    let content = b"pinned dependency\n".to_vec();
    let content_hash = oak_core::hash_bytes(&content);
    let empty_hash = Blob::empty_hash();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/acme/widget/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "blobs": [
                {
                    "hash": content_hash.as_str(),
                    "size": content.len(),
                    "chunks": [{
                        "hash": content_hash.as_str(),
                        "offset": 0,
                        "size": content.len(),
                    }],
                },
                // The metadata-only row, advertised with nothing behind it.
                { "hash": empty_hash.as_str(), "size": 0, "chunks": [] },
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/acme/widget/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{ "hash": content_hash.as_str(), "content": content }]
        })))
        .mount(&server)
        .await;

    oak_cli::commands::blob_fetch::ensure_blobs_local(
        &repo,
        &server.uri(),
        "acme",
        "widget",
        None,
        std::slice::from_ref(&content_hash),
    )
    .await
    .expect("a chunkless empty blob in the reply must not fail the fetch");

    assert_eq!(
        repo.get_blob(&content_hash).unwrap().unwrap().content,
        content
    );
    assert!(repo.has_blob(&empty_hash).unwrap());
}
