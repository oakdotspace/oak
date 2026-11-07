//! Perforce-style partial checkouts end to end: a sparse cone (a p4 client
//! view) combined with server-withheld ("restricted") content (p4
//! protections). Pins the hardening that keeps both from losing work or
//! wedging the checkout:
//!
//! - changing the cone refuses while uncommitted or untracked work sits on
//!   either side of the change, so a re-sync never overwrites or deletes it;
//! - `oak sparse add` downloads what enters the cone (tolerating content the
//!   server withholds) or refuses without changing anything;
//! - `oak pull` with nothing new upstream still hydrates never-downloaded and
//!   newly granted files;
//! - `oak restore` / `oak reset` stay inside the cone and report withheld
//!   content with the access hint instead of "incomplete data";
//! - `oak commit <path>` outside the cone is an error.

use std::fs;
use std::path::Path;

use oak_cli::commands::sparse::SparseAction;
use oak_core::{Hash, MetadataKey, OakError, Repository, SqliteRepository};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn init_repo(dir: &Path) {
    unsafe { std::env::set_var("OAK_AUTHOR", "tester") };
    oak_cli::commands::init::run(dir, false).unwrap();
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

fn sparse(dir: &Path, action: SparseAction) -> oak_core::Result<()> {
    oak_cli::commands::sparse::run(dir, action)
}

fn set(paths: &[&str]) -> SparseAction {
    SparseAction::Set {
        paths: paths.iter().map(|p| p.to_string()).collect(),
    }
}

fn add(paths: &[&str]) -> SparseAction {
    SparseAction::Add {
        paths: paths.iter().map(|p| p.to_string()).collect(),
    }
}

fn cone(dir: &Path) -> Option<String> {
    open_repo(dir)
        .get_metadata(MetadataKey::SparsePaths)
        .unwrap()
        .filter(|v| !v.is_empty())
}

/// Simulate a sparse clone that never downloaded `content`: drop its blob row.
fn forget_blob(dir: &Path, content: &[u8]) -> Hash {
    let hash = oak_core::hash_bytes(content);
    let db = rusqlite::Connection::open(dir.join(".oak/oak.db")).unwrap();
    db.execute("DELETE FROM blobs WHERE hash = ?1", [hash.as_str()])
        .unwrap();
    db.execute(
        "DELETE FROM blob_chunks WHERE blob_hash = ?1",
        [hash.as_str()],
    )
    .unwrap();
    hash
}

/// `src/a.txt` + `docs/d.txt` committed, then scoped to `src`.
fn sparse_fixture(dir: &Path) {
    init_repo(dir);
    write_file(dir, "src/a.txt", "a\n");
    write_file(dir, "docs/d.txt", "d\n");
    oak_cli::commands::commit::run(dir).unwrap();
    sparse(dir, set(&["src"])).unwrap();
    assert!(!dir.join("docs").exists());
}

fn link_remote(dir: &Path, uri: &str) {
    let repo = open_repo(dir);
    repo.set_metadata(MetadataKey::RemoteUrl, uri).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
}

/// A `/blobs/info` mock: `inline` blobs ship their bytes, `withheld` hashes
/// come back as restricted.
async fn blob_info(server: &MockServer, inline: &[&[u8]], withheld: &[&Hash]) {
    let blobs: Vec<_> = inline
        .iter()
        .map(|content| {
            json!({
                "hash": oak_core::hash_bytes(content).as_str(),
                "content": content.to_vec(),
                "size": content.len(),
                "chunks": [],
            })
        })
        .collect();
    let restricted: Vec<&str> = withheld.iter().map(|h| h.as_str()).collect();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": blobs,
            "restricted": restricted,
        })))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Gap 1: cone changes never overwrite or delete work
// ---------------------------------------------------------------------------

#[test]
fn disable_refuses_out_of_cone_edits_and_untracked_files() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);

    // Someone works outside the cone: edits a tracked file, creates a new one.
    write_file(root, "docs/d.txt", "edited outside the cone\n");
    write_file(root, "docs/new.txt", "untracked\n");

    let err = sparse(root, SparseAction::Disable).expect_err("must refuse");
    assert!(matches!(err, OakError::DirtyWorkingTree(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("docs/d.txt") && msg.contains("docs/new.txt"),
        "{msg}"
    );

    // Nothing moved: the cone and both files are exactly as the user left them.
    assert_eq!(cone(root).as_deref(), Some("src"));
    assert_eq!(read(root, "docs/d.txt"), "edited outside the cone\n");
    assert_eq!(read(root, "docs/new.txt"), "untracked\n");
}

#[test]
fn add_refuses_edits_under_the_added_prefix() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);
    write_file(root, "docs/d.txt", "mine\n");

    let err = sparse(root, add(&["docs"])).expect_err("must refuse");
    assert!(matches!(err, OakError::DirtyWorkingTree(_)), "{err:?}");
    assert!(err.to_string().contains("docs/d.txt"), "{err}");
    assert_eq!(read(root, "docs/d.txt"), "mine\n");
    assert_eq!(cone(root).as_deref(), Some("src"));

    // Unrelated out-of-cone files elsewhere don't block the change.
    fs::remove_file(root.join("docs/d.txt")).unwrap();
    write_file(root, "scratch/notes.txt", "kept\n");
    sparse(root, add(&["docs"])).unwrap();
    assert_eq!(read(root, "docs/d.txt"), "d\n");
    assert_eq!(read(root, "scratch/notes.txt"), "kept\n");
}

#[test]
fn set_refuses_dirty_files_leaving_the_cone() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);
    sparse(root, add(&["docs"])).unwrap();
    write_file(root, "docs/d.txt", "edited\n");

    let err = sparse(root, set(&["src"])).expect_err("must refuse");
    assert!(matches!(err, OakError::DirtyWorkingTree(_)), "{err:?}");
    assert_eq!(read(root, "docs/d.txt"), "edited\n");
}

// ---------------------------------------------------------------------------
// Gap 3: `oak sparse add` hydrates, or refuses without wedging the checkout
// ---------------------------------------------------------------------------

#[test]
fn add_without_content_or_remote_refuses_and_leaves_status_working() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);
    forget_blob(root, b"d\n");

    let err = sparse(root, add(&["docs"])).expect_err("nothing to hydrate from");
    assert!(err.to_string().contains("docs/d.txt"), "{err}");
    assert!(err.to_string().contains("cone was not changed"), "{err}");
    assert_eq!(cone(root).as_deref(), Some("src"));

    // The old failure mode wedged status/commit/sparse with IncompleteBlobData.
    assert!(oak_cli::commands::commit::get_status(root)
        .unwrap()
        .0
        .is_empty());
    sparse(root, set(&["src"])).unwrap();
}

#[test]
fn add_downloads_entering_content_and_skips_withheld_files() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(MockServer::start());
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    init_repo(root);
    write_file(root, "src/a.txt", "a\n");
    write_file(root, "docs/d.txt", "d\n");
    write_file(root, "docs/secret/s.txt", "s3cret\n");
    oak_cli::commands::commit::run(root).unwrap();
    sparse(root, set(&["src"])).unwrap();
    forget_blob(root, b"d\n");
    let secret = forget_blob(root, b"s3cret\n");
    link_remote(root, &server.uri());
    rt.block_on(blob_info(&server, &[b"d\n"], &[&secret]));

    sparse(root, add(&["docs"])).unwrap();

    assert_eq!(cone(root).as_deref(), Some("docs\nsrc"));
    assert_eq!(read(root, "docs/d.txt"), "d\n");
    assert!(!root.join("docs/secret/s.txt").exists());
    let repo = open_repo(root);
    assert!(oak_cli::commands::restricted::load_restricted_blobs(&repo).contains(secret.as_str()));
    drop(repo);
    // The withheld file is not a deletion; the hydrated one is clean.
    assert!(oak_cli::commands::commit::get_status(root)
        .unwrap()
        .0
        .is_empty());
}

// ---------------------------------------------------------------------------
// Gaps 2/3: `oak pull` with an unchanged head hydrates granted content
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn pull_with_nothing_new_hydrates_a_fresh_grant() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    init_repo(root);
    write_file(root, "src/a.txt", "a\n");
    write_file(root, "secret/key.txt", "s3cret\n");
    write_file(root, "secret/other.txt", "still hidden\n");
    oak_cli::commands::commit::run(root).unwrap();
    let repo = open_repo(root);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    drop(repo);
    let granted = forget_blob(root, b"s3cret\n");
    let hidden = forget_blob(root, b"still hidden\n");
    fs::remove_file(root.join("secret/key.txt")).unwrap();
    fs::remove_file(root.join("secret/other.txt")).unwrap();
    oak_cli::commands::restricted::record_restricted_blobs(
        &open_repo(root),
        &[granted.to_string(), hidden.to_string()],
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": head.as_str(),
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": [],
            "restricted_blobs": [],
        })))
        .mount(&server)
        .await;
    // An admin granted `key.txt`; `other.txt` is still withheld.
    blob_info(&server, &[b"s3cret\n"], &[&hidden]).await;
    link_remote(root, &server.uri());

    let repo = open_repo(root);
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&root.join(".oak")).unwrap();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some(&branch),
        Some(&head),
        false,
        root,
        None,
    )
    .await
    .unwrap();
    drop(lock);

    assert_eq!(read(root, "secret/key.txt"), "s3cret\n");
    assert!(!root.join("secret/other.txt").exists());
    let restricted = oak_cli::commands::restricted::load_restricted_blobs(&repo);
    assert!(
        !restricted.contains(granted.as_str()),
        "written => marker spent"
    );
    assert!(
        restricted.contains(hidden.as_str()),
        "still withheld => kept"
    );
    drop(repo);
    assert!(oak_cli::commands::commit::get_status(root)
        .unwrap()
        .0
        .is_empty());
}

// ---------------------------------------------------------------------------
// Gap 5: restore / reset respect the cone and withheld content
// ---------------------------------------------------------------------------

#[test]
fn restore_and_reset_skip_withheld_content_with_the_access_hint() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    init_repo(root);
    write_file(root, "src/a.txt", "a\n");
    write_file(root, "secret/key.txt", "s3cret\n");
    oak_cli::commands::commit::run(root).unwrap();
    let secret = forget_blob(root, b"s3cret\n");
    fs::remove_file(root.join("secret/key.txt")).unwrap();
    oak_cli::commands::restricted::record_restricted_blobs(&open_repo(root), &[secret.to_string()])
        .unwrap();

    // Whole-tree restore/reset still restore everything else.
    write_file(root, "src/a.txt", "dirty\n");
    oak_cli::commands::restore::run(root, &[], None, true).unwrap();
    assert_eq!(read(root, "src/a.txt"), "a\n");
    write_file(root, "src/a.txt", "dirty\n");
    oak_cli::commands::reset::run(root, None, true).unwrap();
    assert_eq!(read(root, "src/a.txt"), "a\n");

    // Naming the withheld file is an access error, not "incomplete data".
    let err = oak_cli::commands::restore::run(root, &[root.join("secret/key.txt")], None, true)
        .expect_err("withheld content can't be restored");
    assert!(matches!(err, OakError::RestrictedContent(_)), "{err:?}");
    assert!(err.to_string().contains("ask an org admin"), "{err}");
    let err = oak_cli::commands::reset::run(root, Some(&root.join("secret")), true)
        .expect_err("withheld content can't be reset");
    assert!(matches!(err, OakError::RestrictedContent(_)), "{err:?}");
}

#[test]
fn restore_stays_inside_the_cone() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);
    write_file(root, "docs/d.txt", "outside\n");

    // A whole-tree restore doesn't touch (or claim to restore) out-of-cone
    // paths: it reports a clean tree and leaves the file alone.
    oak_cli::commands::restore::run(root, &[], None, true).unwrap();
    assert_eq!(read(root, "docs/d.txt"), "outside\n");
    oak_cli::commands::reset::run(root, None, true).unwrap();
    assert_eq!(read(root, "docs/d.txt"), "outside\n");

    let err = oak_cli::commands::restore::run(root, &[root.join("docs")], None, true)
        .expect_err("out-of-cone restore target");
    assert!(err.to_string().contains("oak sparse add docs"), "{err}");
}

// ---------------------------------------------------------------------------
// Gap 8: commits name out-of-cone paths instead of silently dropping them
// ---------------------------------------------------------------------------

#[test]
fn commit_of_an_out_of_cone_path_is_an_error() {
    let temp = TempDir::new().unwrap();
    let root = &temp.path().canonicalize().unwrap();
    sparse_fixture(root);
    write_file(root, "docs/new.txt", "new\n");

    let err = oak_cli::commands::commit::run_with_options(
        root,
        oak_cli::commands::commit::CommitOptions {
            paths: vec![root.join("docs/new.txt")],
            ..Default::default()
        },
    )
    .expect_err("out-of-cone commit path");
    assert!(matches!(err, OakError::InvalidArgument(_)), "{err:?}");
    assert!(
        err.to_string().contains("oak sparse add docs/new.txt"),
        "{err}"
    );

    // In-cone paths still commit; out-of-cone files are carried, not recorded.
    write_file(root, "src/a.txt", "edited\n");
    oak_cli::commands::commit::run(root).unwrap();
    let repo = open_repo(root);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    assert!(manifest.get("docs/new.txt").is_none());
    assert!(manifest.get("docs/d.txt").is_some());
}
