//! Sparse-checkout (Perforce-style partial clone) behavior at the command
//! level. These exercise the *local* half — cone storage, working-tree
//! re-sync, and the commit/status carry-forward that keeps out-of-cone paths
//! from being erased — without needing a live server. The server-side blob
//! filtering on `pull` is covered in the oakspace repo's `path_permissions`
//! suite (same omit-blob mechanism).

use std::fs;
use std::path::Path;

use oak_cli::commands::sparse::SparseAction;
use oak_core::{MetadataKey, Repository, SqliteRepository};
use tempfile::TempDir;

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

/// The entries of the current branch's HEAD manifest, sorted by path.
fn head_paths(dir: &Path) -> Vec<String> {
    let repo = open_repo(dir);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let mut paths: Vec<String> = manifest.entries.iter().map(|e| e.path.clone()).collect();
    paths.sort();
    paths
}

fn head_blob_hash(dir: &Path, path: &str) -> String {
    let repo = open_repo(dir);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    manifest.get(path).unwrap().blob_hash.to_string()
}

fn current_branch_and_head(dir: &Path) -> (String, String) {
    let repo = open_repo(dir);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    (branch, head.to_string())
}

#[test]
fn sparse_set_scopes_tree_and_commit_carries_forward() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_repo(root);

    write_file(root, "src/app.rs", "fn main() {}\n");
    write_file(root, "docs/guide.md", "# Guide\n");
    write_file(root, "README.md", "hello\n");
    oak_cli::commands::commit::run(root).unwrap();

    // Scope to `src/` — this should drop the out-of-cone files from disk while
    // keeping them in the repo.
    oak_cli::commands::sparse::run(
        root,
        SparseAction::Set {
            paths: vec!["src".to_string()],
        },
    )
    .unwrap();

    assert!(root.join("src/app.rs").exists(), "in-cone file kept");
    assert!(
        !root.join("docs/guide.md").exists(),
        "out-of-cone file removed from disk"
    );
    assert!(
        !root.join("README.md").exists(),
        "out-of-cone root file removed from disk"
    );

    // The cone is persisted.
    let repo = open_repo(root);
    assert_eq!(
        repo.get_metadata(MetadataKey::SparsePaths)
            .unwrap()
            .as_deref(),
        Some("src")
    );

    // Status must be clean: the missing out-of-cone files are NOT phantom
    // deletions.
    assert!(
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, root).unwrap(),
        "sparse working tree should read as clean"
    );

    // Edit an in-cone file and commit. The new HEAD must still carry the
    // out-of-cone paths forward verbatim — a sparse commit never erases them.
    write_file(root, "src/app.rs", "fn main() { /* edit */ }\n");
    oak_cli::commands::commit::run(root).unwrap();

    assert_eq!(
        head_paths(root),
        vec![
            "README.md".to_string(),
            "docs/guide.md".to_string(),
            "src/app.rs".to_string(),
        ],
        "out-of-cone entries carried forward into the sparse commit"
    );
}

#[test]
fn sparse_status_and_commit_accept_missing_out_of_cone_bytes_and_preserve_identity() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_repo(root);

    write_file(root, "src/app.rs", "fn main() {}\n");
    write_file(root, "docs/guide.md", "# Guide\n");
    oak_cli::commands::commit::run(root).unwrap();
    let excluded_hash = head_blob_hash(root, "docs/guide.md");

    oak_cli::commands::sparse::run(
        root,
        SparseAction::Set {
            paths: vec!["src".to_string()],
        },
    )
    .unwrap();

    let connection = rusqlite::Connection::open(root.join(".oak/oak.db")).unwrap();
    connection
        .execute("DELETE FROM blobs WHERE hash = ?1", [&excluded_hash])
        .unwrap();
    drop(connection);

    let repo = open_repo(root);
    assert!(
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, root).unwrap(),
        "deliberately absent out-of-cone bytes must not make status incomplete"
    );
    drop(repo);

    write_file(root, "src/app.rs", "fn main() { /* sparse edit */ }\n");
    oak_cli::commands::commit::run(root).unwrap();

    assert_eq!(
        head_blob_hash(root, "docs/guide.md"),
        excluded_hash,
        "sparse commit must carry the excluded path's exact blob identity"
    );
}

#[test]
fn sparse_switch_does_not_hydrate_or_materialize_out_of_cone_bytes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_repo(root);

    write_file(root, "src/app.rs", "base\n");
    write_file(root, "docs/guide.md", "excluded\n");
    oak_cli::commands::commit::run(root).unwrap();
    let excluded_hash = head_blob_hash(root, "docs/guide.md");
    let (base_branch, base_head) = current_branch_and_head(root);

    oak_cli::commands::branch::new_branch(root, "feature", None, Some(&base_head), None).unwrap();
    write_file(root, "src/app.rs", "feature\n");
    oak_cli::commands::commit::run(root).unwrap();
    oak_cli::commands::switch::run(root, Some(&base_branch), false).unwrap();

    oak_cli::commands::sparse::run(
        root,
        SparseAction::Set {
            paths: vec!["src".to_string()],
        },
    )
    .unwrap();
    let connection = rusqlite::Connection::open(root.join(".oak/oak.db")).unwrap();
    connection
        .execute("DELETE FROM blobs WHERE hash = ?1", [&excluded_hash])
        .unwrap();
    drop(connection);

    oak_cli::commands::switch::run(root, Some("feature"), false).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("src/app.rs")).unwrap(),
        "feature\n"
    );
    assert!(!root.join("docs/guide.md").exists());
    assert_eq!(head_blob_hash(root, "docs/guide.md"), excluded_hash);
}

#[test]
fn sparse_add_widens_then_disable_restores_full_tree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_repo(root);

    write_file(root, "src/app.rs", "a\n");
    write_file(root, "docs/guide.md", "b\n");
    write_file(root, "infra/main.tf", "c\n");
    oak_cli::commands::commit::run(root).unwrap();

    oak_cli::commands::sparse::run(
        root,
        SparseAction::Set {
            paths: vec!["src".to_string()],
        },
    )
    .unwrap();
    assert!(!root.join("docs/guide.md").exists());
    assert!(!root.join("infra/main.tf").exists());

    // Widen the cone to include docs/. Its blob is still in the local store
    // (this is a local repo, nothing was withheld), so it re-materializes.
    oak_cli::commands::sparse::run(
        root,
        SparseAction::Add {
            paths: vec!["docs".to_string()],
        },
    )
    .unwrap();
    assert!(root.join("src/app.rs").exists());
    assert!(
        root.join("docs/guide.md").exists(),
        "widened cone hydrates docs/"
    );
    assert!(
        !root.join("infra/main.tf").exists(),
        "infra/ still excluded"
    );

    // Disable: full checkout restored.
    oak_cli::commands::sparse::run(root, SparseAction::Disable).unwrap();
    assert!(root.join("src/app.rs").exists());
    assert!(root.join("docs/guide.md").exists());
    assert!(
        root.join("infra/main.tf").exists(),
        "disable restores full tree"
    );

    let repo = open_repo(root);
    // Empty value reads back as "no cone" (full checkout).
    assert!(
        oak_core::SparseCone::from_metadata(
            repo.get_metadata(MetadataKey::SparsePaths)
                .unwrap()
                .as_deref()
        )
        .is_none(),
        "cone cleared after disable"
    );
}

#[test]
fn sparse_set_refuses_dirty_tree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    init_repo(root);
    write_file(root, "src/app.rs", "a\n");
    oak_cli::commands::commit::run(root).unwrap();

    // Uncommitted edit in the cone.
    write_file(root, "src/app.rs", "dirty\n");

    let err = oak_cli::commands::sparse::run(
        root,
        SparseAction::Set {
            paths: vec!["src".to_string()],
        },
    )
    .expect_err("changing the cone with a dirty tree must fail");
    assert!(
        err.to_string().contains("uncommitted"),
        "expected a dirty-tree error, got: {err}"
    );
}
