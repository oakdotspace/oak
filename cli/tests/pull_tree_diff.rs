//! `oak pull`'s branch fast-forward moves the working tree by a tree diff of
//! the old and new heads instead of rewriting every file of the new head.
//! These tests pin what that buys and what it must keep:
//!
//! - the clean guard and the update read only the directories that differ,
//!   so a pull succeeds when untouched trees and blobs are missing locally;
//! - `--force` still rewrites every file, so it still overwrites uncommitted
//!   edits to files the pull didn't change;
//! - content withheld earlier that arrives still can't be skipped: the
//!   guard refuses, and `--force` lands it;
//! - a missing blob the update must write still fails, with the pull's own
//!   message, before any file or ref moves; restricted content is skipped.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use oak_core::{Commit, Hash, ManifestEntry, MetadataKey, OakError};
use oak_core::{Repository, SqliteRepository};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BRANCH: &str = "tester";

fn init_repo(dir: &Path) {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    repo.store_branch(&oak_core::Branch::new(
        BRANCH.to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(BRANCH).unwrap();
}

fn open_repo(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn write_file(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap()
}

fn head_root(repo: &SqliteRepository) -> Hash {
    let head = repo.get_branch_head(BRANCH).unwrap().unwrap();
    repo.get_commit(&head).unwrap().unwrap().manifest_hash
}

fn head_entries(repo: &SqliteRepository) -> Vec<ManifestEntry> {
    repo.get_manifest(&head_root(repo))
        .unwrap()
        .unwrap()
        .entries
}

/// Replace (or add) `path` in `entries` with a regular file holding `blob`.
fn with(entries: &[ManifestEntry], path: &str, blob: &Hash) -> Vec<ManifestEntry> {
    let mut out: Vec<ManifestEntry> = entries.iter().filter(|e| e.path != path).cloned().collect();
    out.push(ManifestEntry {
        path: path.to_string(),
        blob_hash: blob.clone(),
        mode: oak_core::FileMode::Regular,
    });
    out
}

fn without(entries: &[ManifestEntry], path: &str) -> Vec<ManifestEntry> {
    entries.iter().filter(|e| e.path != path).cloned().collect()
}

fn delete_rows(dir: &Path, table: &str, hashes: &[Hash]) {
    let db = rusqlite::Connection::open(dir.join(".oak/oak.db")).unwrap();
    for hash in hashes {
        let deleted = db
            .execute(
                &format!("DELETE FROM {table} WHERE hash = ?1"),
                [hash.as_str()],
            )
            .unwrap();
        assert_eq!(deleted, 1, "{table} row {hash} should exist");
    }
}

/// An upstream commit on the branch, child of `old_head`, whose tree is
/// stored locally (the fast-forward shape: the response ships no trees).
struct Upstream {
    head: Hash,
    root: Hash,
    parent: Hash,
    timestamp: DateTime<Utc>,
}

fn upstream(repo: &SqliteRepository, old_head: &Hash, entries: Vec<ManifestEntry>) -> Upstream {
    let root = repo.put_manifest(entries).unwrap();
    let timestamp = DateTime::from_timestamp(1_700_000_500, 0).unwrap();
    let commit = Commit::with_timestamp(
        BRANCH.to_string(),
        Some(old_head.clone()),
        None,
        root.clone(),
        "peer".to_string(),
        None,
        Vec::new(),
        timestamp,
    )
    .unwrap();
    Upstream {
        head: commit.hash,
        root,
        parent: old_head.clone(),
        timestamp,
    }
}

/// Answer one pull with `upstream`, shipping `blobs` and naming
/// `restricted` as withheld.
async fn serve(upstream: &Upstream, blobs: Vec<Value>, restricted: Vec<String>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": upstream.head.as_str(),
            "branch": null,
            "branches": [{
                "name": BRANCH,
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            }],
            "commits": [{
                "hash": upstream.head.as_str(),
                "branch_name": BRANCH,
                "parent_hash": upstream.parent.to_string(),
                "manifest_hash": upstream.root.to_string(),
                "author": "peer",
                "timestamp": upstream.timestamp.to_rfc3339(),
                "files": [],
            }],
            "blobs": blobs,
            "trees": [],
            "restricted_blobs": restricted,
        })))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn pull(
    dir: &Path,
    server: &MockServer,
    old_head: &Hash,
    force: bool,
) -> oak_core::Result<()> {
    let repo = open_repo(dir);
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&dir.join(".oak")).unwrap();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some(BRANCH),
        Some(old_head),
        force,
        dir,
        None,
    )
    .await
}

/// Eight directories of four files each, committed.
fn commit_wide(dir: &Path) -> Hash {
    for d in 0..8 {
        for f in 0..4 {
            write_file(dir, &format!("d{d}/f{f}.txt"), &format!("d{d}f{f}\n"));
        }
    }
    oak_cli::commands::commit::run(dir).unwrap();
    open_repo(dir).get_branch_head(BRANCH).unwrap().unwrap()
}

/// A one-directory upstream change fast-forwards without reading anything
/// else: `d5`'s tree and the blobs of `d5` and `d6` are gone locally, which
/// made the flattened pull (and its clean guard) fail. Only the changed
/// files are written; an upstream deletion is applied and its stat-cache row
/// pruned.
#[tokio::test(flavor = "current_thread")]
async fn fast_forward_reads_only_what_changed() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let old_head = commit_wide(temp.path());
    let repo = open_repo(temp.path());
    let base = head_entries(&repo);

    let edited = repo.put_blob(b"upstream edit\n".to_vec()).unwrap();
    let added = repo.put_blob(b"upstream add\n".to_vec()).unwrap();
    let up = upstream(
        &repo,
        &old_head,
        with(
            &without(&with(&base, "d1/f0.txt", &edited), "d2/f1.txt"),
            "d3/new.txt",
            &added,
        ),
    );
    let d5 = repo.subtree_at_path(&up.root, "d5").unwrap().unwrap();
    assert_eq!(
        repo.subtree_at_path(&head_root(&repo), "d5").unwrap(),
        Some(d5.clone())
    );
    let untouched: Vec<Hash> = base
        .iter()
        .filter(|e| e.path.starts_with("d5/") || e.path.starts_with("d6/"))
        .map(|e| e.blob_hash.clone())
        .collect();
    drop(repo);
    delete_rows(temp.path(), "trees", std::slice::from_ref(&d5));
    delete_rows(temp.path(), "blobs", &untouched);

    let server = serve(&up, Vec::new(), Vec::new()).await;
    pull(temp.path(), &server, &old_head, false).await.unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(repo.get_branch_head(BRANCH).unwrap(), Some(up.head));
    assert_eq!(read(temp.path(), "d1/f0.txt"), "upstream edit\n");
    assert_eq!(read(temp.path(), "d3/new.txt"), "upstream add\n");
    assert!(!temp.path().join("d2/f1.txt").exists());
    assert_eq!(read(temp.path(), "d5/f0.txt"), "d5f0\n");
    assert_eq!(read(temp.path(), "d6/f3.txt"), "d6f3\n");
    assert!(
        repo.get_tree(&d5).unwrap().is_none(),
        "the pull never needed d5"
    );
    let cache = repo.load_stat_cache().unwrap();
    assert!(!cache.contains_key("d2/f1.txt"));
    assert_eq!(cache["d1/f0.txt"].blob_hash, edited);
}

/// `--force` overwrites uncommitted work, so it still rewrites every file of
/// the new head — including one the pull didn't change — while leaving
/// untracked files alone.
#[tokio::test(flavor = "current_thread")]
async fn force_rewrites_files_the_pull_did_not_change() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "a\n");
    write_file(temp.path(), "b/c.txt", "c\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let old_head = repo.get_branch_head(BRANCH).unwrap().unwrap();
    let edited = repo.put_blob(b"upstream a\n".to_vec()).unwrap();
    let up = upstream(
        &repo,
        &old_head,
        with(&head_entries(&repo), "a.txt", &edited),
    );
    drop(repo);

    write_file(temp.path(), "b/c.txt", "uncommitted edit\n");
    write_file(temp.path(), "notes.txt", "untracked\n");

    let refused = serve(&up, Vec::new(), Vec::new()).await;
    let err = pull(temp.path(), &refused, &old_head, false)
        .await
        .expect_err("a dirty tree refuses an ordinary pull");
    assert!(matches!(err, OakError::DirtyWorkingTree(_)), "got {err:?}");
    assert_eq!(read(temp.path(), "b/c.txt"), "uncommitted edit\n");

    let forced = serve(&up, Vec::new(), Vec::new()).await;
    pull(temp.path(), &forced, &old_head, true).await.unwrap();
    assert_eq!(read(temp.path(), "a.txt"), "upstream a\n");
    assert_eq!(read(temp.path(), "b/c.txt"), "c\n");
    assert_eq!(read(temp.path(), "notes.txt"), "untracked\n");
    assert_eq!(
        open_repo(temp.path()).get_branch_head(BRANCH).unwrap(),
        Some(up.head)
    );
}

/// A file whose content the server withheld (restricted) was never written.
/// A pull that brings its blob can't land it by tree diff — the file is the
/// same in both heads — and doesn't need to: the clean guard runs after the
/// download, sees the absent file as a deletion, and refuses, as the
/// flattened pull did. `--force` rewrites every file and lands it.
#[tokio::test(flavor = "current_thread")]
async fn withheld_content_that_arrives_lands_with_force() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "a\n");
    write_file(temp.path(), "secret/key.txt", "s3cret\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let old_head = repo.get_branch_head(BRANCH).unwrap().unwrap();
    let secret = oak_core::hash_bytes(b"s3cret\n");
    repo.set_metadata(MetadataKey::RestrictedBlobs, secret.as_str())
        .unwrap();
    let edited = repo.put_blob(b"upstream a\n".to_vec()).unwrap();
    let up = upstream(
        &repo,
        &old_head,
        with(&head_entries(&repo), "a.txt", &edited),
    );
    drop(repo);
    delete_rows(temp.path(), "blobs", std::slice::from_ref(&secret));
    fs::remove_file(temp.path().join("secret/key.txt")).unwrap();
    assert!(
        oak_cli::commands::commit::get_status(temp.path())
            .unwrap()
            .0
            .is_empty(),
        "a withheld file's absence is not a change"
    );

    // Access granted: the response ships the blob (its one chunk is already
    // local, so no download request is made) and withholds nothing.
    open_repo(temp.path())
        .store_chunk(&secret, b"s3cret\n")
        .unwrap();
    let blob = json!({
        "hash": secret.as_str(),
        "content": [],
        "size": 7,
        "chunks": [{"hash": secret.as_str(), "offset": 0, "size": 7}],
    });
    let plain = serve(&up, vec![blob.clone()], Vec::new()).await;
    let err = pull(temp.path(), &plain, &old_head, false)
        .await
        .expect_err("the arrived file reads as deleted");
    assert!(matches!(err, OakError::DirtyWorkingTree(_)), "got {err:?}");
    assert_eq!(
        open_repo(temp.path()).get_branch_head(BRANCH).unwrap(),
        Some(old_head.clone())
    );

    let forced = serve(&up, vec![blob], Vec::new()).await;
    pull(temp.path(), &forced, &old_head, true).await.unwrap();
    assert_eq!(read(temp.path(), "a.txt"), "upstream a\n");
    assert_eq!(read(temp.path(), "secret/key.txt"), "s3cret\n");
}

/// An upstream file the server withholds as restricted is skipped, not a
/// failed pull: the rest of the change lands and the head moves.
#[tokio::test(flavor = "current_thread")]
async fn restricted_upstream_file_is_skipped() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "a\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let old_head = repo.get_branch_head(BRANCH).unwrap().unwrap();
    let edited = repo.put_blob(b"upstream a\n".to_vec()).unwrap();
    let secret = oak_core::hash_bytes(b"withheld\n");
    let up = upstream(
        &repo,
        &old_head,
        with(
            &with(&head_entries(&repo), "a.txt", &edited),
            "secret/new.txt",
            &secret,
        ),
    );
    drop(repo);

    let server = serve(&up, Vec::new(), vec![secret.to_string()]).await;
    pull(temp.path(), &server, &old_head, false).await.unwrap();

    assert_eq!(read(temp.path(), "a.txt"), "upstream a\n");
    assert!(!temp.path().join("secret/new.txt").exists());
    assert_eq!(
        open_repo(temp.path()).get_branch_head(BRANCH).unwrap(),
        Some(up.head)
    );
}

/// A changed file whose blob the server didn't ship fails the pull with the
/// pull's own message, naming the path, before any file is written or the
/// head moves.
#[tokio::test(flavor = "current_thread")]
async fn missing_written_blob_fails_before_anything_moves() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "a\n");
    write_file(temp.path(), "b.txt", "b\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let old_head = repo.get_branch_head(BRANCH).unwrap().unwrap();
    let added = repo.put_blob(b"added\n".to_vec()).unwrap();
    let never_shipped = oak_core::hash_bytes(b"never shipped\n");
    let up = upstream(
        &repo,
        &old_head,
        with(
            &with(&head_entries(&repo), "b.txt", &never_shipped),
            "c.txt",
            &added,
        ),
    );
    drop(repo);

    let server = serve(&up, Vec::new(), Vec::new()).await;
    let err = pull(temp.path(), &server, &old_head, false)
        .await
        .expect_err("an unshipped blob must fail the pull");
    let message = err.to_string();
    assert!(
        message.contains(&format!("pull is missing blob {never_shipped} for 'b.txt'")),
        "got {message}"
    );
    assert!(
        message.contains("OAK_ALLOW_PARTIAL_CLONE=1"),
        "got {message}"
    );
    assert_eq!(
        open_repo(temp.path()).get_branch_head(BRANCH).unwrap(),
        Some(old_head)
    );
    assert_eq!(read(temp.path(), "b.txt"), "b\n");
    assert!(!temp.path().join("c.txt").exists());
}
