//! The local merge flows — `oak pull`'s parent sync and `oak merge` into a
//! local parent — merge root trees instead of flattened manifests. These
//! tests pin what that buys and what it must keep:
//!
//! - a merge reads only the directories and blobs it needs, so it succeeds
//!   when untouched subtrees or their blobs are missing locally;
//! - the working tree moves by the tree diff, leaving untouched and
//!   untracked files alone;
//! - conflicts, sparse cones, and missing data still fail or pause exactly
//!   where they must, before any ref or file moves.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use oak_core::{FileMode, Hash, ManifestEntry, MetadataKey, OakError};
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

fn init_repo(dir: &Path) -> SqliteRepository {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoName, "tree-merge-test")
        .unwrap();
    // A local (non-main) parent keeps every flow offline.
    repo.store_branch(&oak_core::Branch::new("dev".to_string(), None, None))
        .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("dev".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("feature").unwrap();
    repo
}

fn open_repo(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn entry(path: &str, blob: &Hash) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        blob_hash: blob.clone(),
        mode: FileMode::Regular,
    }
}

/// Replace (or add) `path` in `entries`.
fn with(entries: &[ManifestEntry], path: &str, blob: &Hash) -> Vec<ManifestEntry> {
    let mut out: Vec<ManifestEntry> = entries.iter().filter(|e| e.path != path).cloned().collect();
    out.push(entry(path, blob));
    out
}

fn without(entries: &[ManifestEntry], path: &str) -> Vec<ManifestEntry> {
    entries.iter().filter(|e| e.path != path).cloned().collect()
}

/// Eight directories of four files each.
fn wide(repo: &SqliteRepository) -> Vec<ManifestEntry> {
    let mut entries = Vec::new();
    for d in 0..8 {
        for f in 0..4 {
            let blob = repo.put_blob(format!("d{d}f{f}\n").into_bytes()).unwrap();
            entries.push(entry(&format!("d{d}/f{f}.txt"), &blob));
        }
    }
    entries
}

fn commit(
    repo: &SqliteRepository,
    branch: &str,
    parent: Option<&Hash>,
    entries: Vec<ManifestEntry>,
) -> Hash {
    let root = repo.put_manifest(entries).unwrap();
    let hash = repo
        .put_commit(
            branch.to_string(),
            parent.cloned(),
            None,
            root,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head(branch, &hash).unwrap();
    hash
}

/// Point the checkout at `feature`'s head and write its tree to disk.
fn check_out_feature(dir: &Path, repo: &SqliteRepository) {
    let head = repo.get_branch_head("feature").unwrap().unwrap();
    repo.set_head(&head).unwrap();
    let root = repo.get_commit(&head).unwrap().unwrap().manifest_hash;
    let manifest = repo.get_manifest(&root).unwrap().unwrap();
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&dir.join(".oak")).unwrap();
    oak_cli::commands::switch::update_working_dir(&lock, dir, repo, &manifest).unwrap();
}

fn root_of(repo: &SqliteRepository, commit: &Hash) -> Hash {
    repo.get_commit(commit).unwrap().unwrap().manifest_hash
}

fn files_at(repo: &SqliteRepository, root: &Hash) -> HashMap<String, Hash> {
    repo.get_manifest(root)
        .unwrap()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| (e.path, e.blob_hash))
        .collect()
}

fn subtree(repo: &SqliteRepository, root: &Hash, dir: &str) -> Hash {
    repo.subtree_at_path(root, dir).unwrap().unwrap()
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

fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap()
}

/// A one-sided parent change merges and lands on disk without reading the
/// directories neither side touched: their trees and blobs can be missing
/// locally. The working tree moves by the diff, so an untracked file
/// survives (a whole-tree reset used to delete it).
#[tokio::test(flavor = "current_thread")]
async fn sync_merges_without_reading_untouched_directories() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());

    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    let feature_entries = with(&base_entries, "d1/f0.txt", &branch_blob);
    let feature_head = commit(&repo, "feature", Some(&base), feature_entries.clone());

    let parent_blob = repo.put_blob(b"parent\n".to_vec()).unwrap();
    let added_blob = repo.put_blob(b"added\n".to_vec()).unwrap();
    let dev_entries = without(
        &with(
            &with(&base_entries, "d2/f1.txt", &parent_blob),
            "d2/new.txt",
            &added_blob,
        ),
        "d3/f2.txt",
    );
    let dev_head = commit(&repo, "dev", Some(&base), dev_entries);

    check_out_feature(temp.path(), &repo);
    fs::write(temp.path().join("scratch.txt"), "untracked\n").unwrap();

    // Neither side touched d5 or d6: drop d5's tree and both dirs' blobs.
    let base_root = root_of(&repo, &base);
    let d5 = subtree(&repo, &base_root, "d5");
    let untouched_blobs: Vec<Hash> = base_entries
        .iter()
        .filter(|e| e.path.starts_with("d5/") || e.path.starts_with("d6/"))
        .map(|e| e.blob_hash.clone())
        .collect();
    delete_rows(temp.path(), "trees", std::slice::from_ref(&d5));
    delete_rows(temp.path(), "blobs", &untouched_blobs);
    drop(repo);

    oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head("feature").unwrap().unwrap();
    let synced = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(synced.parent_hash.as_ref(), Some(&feature_head));
    assert_eq!(synced.merge_parent_hash.as_ref(), Some(&dev_head));
    let expected = without(
        &with(
            &with(&feature_entries, "d2/f1.txt", &parent_blob),
            "d2/new.txt",
            &added_blob,
        ),
        "d3/f2.txt",
    );
    assert_eq!(
        synced.manifest_hash,
        oak_core::build_tree(&expected).unwrap().root_hash,
        "the sync result is the flat merge's tree"
    );
    let mut changed: Vec<&str> = synced.files.iter().map(|f| f.path.as_str()).collect();
    changed.sort_unstable();
    assert_eq!(changed, ["d2/f1.txt", "d2/new.txt", "d3/f2.txt"]);
    assert!(
        repo.get_tree(&d5).unwrap().is_none(),
        "the merge never needed d5"
    );

    assert_eq!(read(temp.path(), "d1/f0.txt"), "branch\n");
    assert_eq!(read(temp.path(), "d2/f1.txt"), "parent\n");
    assert_eq!(read(temp.path(), "d2/new.txt"), "added\n");
    assert!(!temp.path().join("d3/f2.txt").exists());
    assert_eq!(read(temp.path(), "d5/f0.txt"), "d5f0\n");
    assert_eq!(read(temp.path(), "scratch.txt"), "untracked\n");

    // Pulling again is a no-op: the parent is now the merge base.
    drop(repo);
    oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap();
    let repo = open_repo(temp.path());
    assert_eq!(repo.get_branch_head("feature").unwrap().unwrap(), head);
}

/// `oak merge` into a local parent: a clean merge commits the merged root on
/// the parent (the flat merge's tree, with the flat diff's file rows) and
/// moves the working tree by the diff.
#[tokio::test(flavor = "current_thread")]
async fn local_merge_moves_worktree_by_tree_diff() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());

    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    let feature_entries = without(&with(&base_entries, "d1/f0.txt", &branch_blob), "d4/f0.txt");
    let feature_head = commit(&repo, "feature", Some(&base), feature_entries.clone());
    let parent_blob = repo.put_blob(b"parent\n".to_vec()).unwrap();
    let dev_head = commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "d7/f3.txt", &parent_blob),
    );
    check_out_feature(temp.path(), &repo);
    drop(repo);

    oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false)
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("dev")
    );
    let head = repo.get_branch_head("dev").unwrap().unwrap();
    let merged = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(merged.parent_hash.as_ref(), Some(&dev_head));
    assert_eq!(merged.merge_parent_hash.as_ref(), Some(&feature_head));
    let expected = with(&feature_entries, "d7/f3.txt", &parent_blob);
    assert_eq!(
        merged.manifest_hash,
        oak_core::build_tree(&expected).unwrap().root_hash
    );
    let mut changed: Vec<&str> = merged.files.iter().map(|f| f.path.as_str()).collect();
    changed.sort_unstable();
    assert_eq!(changed, ["d1/f0.txt", "d4/f0.txt"]);
    assert_eq!(read(temp.path(), "d1/f0.txt"), "branch\n");
    assert_eq!(read(temp.path(), "d7/f3.txt"), "parent\n");
    assert!(!temp.path().join("d4/f0.txt").exists());
}

/// A content conflict pauses the sync with the tree-level result recorded:
/// SYNC_STATE names a stored root holding both sides' clean changes and the
/// markered file, the write-out lands only what the merge changed, and
/// `--continue` commits the resolution plus the deferred deletion.
#[tokio::test(flavor = "current_thread")]
async fn sync_conflict_records_tree_result_and_continue_commits_it() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let app = repo.put_blob(b"a\nb\nc\n".to_vec()).unwrap();
    let base_entries = with(&wide(&repo), "src/lib/app.rs", &app);
    let base = commit(&repo, "dev", None, base_entries.clone());

    let ours = repo.put_blob(b"a\nB1\nc\n".to_vec()).unwrap();
    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    commit(
        &repo,
        "feature",
        Some(&base),
        with(
            &with(&base_entries, "src/lib/app.rs", &ours),
            "d1/f0.txt",
            &branch_blob,
        ),
    );
    let theirs = repo.put_blob(b"a\nB2\nc\n".to_vec()).unwrap();
    let parent_blob = repo.put_blob(b"parent\n".to_vec()).unwrap();
    let dev_head = commit(
        &repo,
        "dev",
        Some(&base),
        without(
            &with(
                &with(&base_entries, "src/lib/app.rs", &theirs),
                "d4/f3.txt",
                &parent_blob,
            ),
            "d3/f2.txt",
        ),
    );
    check_out_feature(temp.path(), &repo);
    let feature_head = repo.get_branch_head("feature").unwrap().unwrap();
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_branch_head("feature").unwrap().unwrap(),
        feature_head
    );
    let state: serde_json::Value =
        serde_json::from_str(&read(temp.path(), ".oak/SYNC_STATE")).unwrap();
    assert_eq!(
        state["conflict_paths"],
        serde_json::json!(["src/lib/app.rs"])
    );
    let recorded = Hash(state["merged_manifest_hash"].as_str().unwrap().to_string());
    let files = files_at(&repo, &recorded);
    assert_eq!(files["d1/f0.txt"], branch_blob);
    assert_eq!(files["d4/f3.txt"], parent_blob);
    assert!(!files.contains_key("d3/f2.txt"));
    let markered = String::from_utf8(
        repo.get_blob(&files["src/lib/app.rs"])
            .unwrap()
            .unwrap()
            .content,
    )
    .unwrap();
    assert!(markered.contains("<<<<<<< feature") && markered.contains(">>>>>>> dev"));

    assert_eq!(read(temp.path(), "src/lib/app.rs"), markered);
    assert_eq!(read(temp.path(), "d4/f3.txt"), "parent\n");
    assert!(
        temp.path().join("d3/f2.txt").exists(),
        "deletions land at --continue"
    );

    fs::write(temp.path().join("src/lib/app.rs"), "a\nB\nc\n").unwrap();
    drop(repo);
    oak_cli::commands::sync::sync_continue(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head("feature").unwrap().unwrap();
    let resolved = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(resolved.merge_parent_hash.as_ref(), Some(&dev_head));
    let files = files_at(&repo, &resolved.manifest_hash);
    assert_eq!(files["src/lib/app.rs"], oak_core::hash_bytes(b"a\nB\nc\n"));
    assert_eq!(files["d4/f3.txt"], parent_blob);
    assert!(!files.contains_key("d3/f2.txt"));
    assert!(!temp.path().join("d3/f2.txt").exists());
    assert!(!temp.path().join(".oak/SYNC_STATE").exists());
}

/// In a sparse checkout the merge carries out-of-cone changes by identity
/// and the working-tree update writes only in-cone files, so neither the
/// out-of-cone base bytes nor the parent's new out-of-cone bytes are needed.
#[tokio::test(flavor = "current_thread")]
async fn sparse_sync_writes_only_in_cone_changes() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    repo.set_metadata(MetadataKey::SparsePaths, "src").unwrap();
    let a = repo.put_blob(b"fn a() {}\n".to_vec()).unwrap();
    let x = repo.put_blob(b"x docs\n".to_vec()).unwrap();
    let y = repo.put_blob(b"y docs\n".to_vec()).unwrap();
    let base_entries = vec![
        entry("src/a.rs", &a),
        entry("docs/x.md", &x),
        entry("docs/y.md", &y),
    ];
    let base = commit(&repo, "dev", None, base_entries.clone());
    let a2 = repo.put_blob(b"fn a() { 2 }\n".to_vec()).unwrap();
    commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "src/a.rs", &a2),
    );
    let b = repo.put_blob(b"fn b() {}\n".to_vec()).unwrap();
    let upstream_x = oak_core::hash_bytes(b"upstream x docs\n");
    commit(
        &repo,
        "dev",
        Some(&base),
        with(
            &with(&base_entries, "src/b.rs", &b),
            "docs/x.md",
            &upstream_x,
        ),
    );
    check_out_feature(temp.path(), &repo);
    delete_rows(temp.path(), "blobs", &[x, y]);
    drop(repo);

    oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head("feature").unwrap().unwrap();
    let files = files_at(&repo, &root_of(&repo, &head));
    assert_eq!(files["docs/x.md"], upstream_x);
    assert_eq!(files["src/a.rs"], a2);
    assert_eq!(files["src/b.rs"], b);
    assert!(!repo.has_blob(&upstream_x).unwrap());
    assert_eq!(read(temp.path(), "src/b.rs"), "fn b() {}\n");
    assert_eq!(read(temp.path(), "src/a.rs"), "fn a() { 2 }\n");
    assert!(!temp.path().join("docs").exists());
}

/// A blob the working-tree update must write is checked before the sync
/// commit is published: the head, HEAD and the tree stay where they were.
#[tokio::test(flavor = "current_thread")]
async fn sync_fails_before_moving_anything_when_a_written_blob_is_missing() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());
    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    let feature_head = commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "d1/f0.txt", &branch_blob),
    );
    let missing = oak_core::hash_bytes(b"never shipped\n");
    let added = repo.put_blob(b"added\n".to_vec()).unwrap();
    commit(
        &repo,
        "dev",
        Some(&base),
        with(
            &with(&base_entries, "d2/f1.txt", &missing),
            "d2/new.txt",
            &added,
        ),
    );
    check_out_feature(temp.path(), &repo);
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(
        matches!(err, OakError::IncompleteBlobData { .. }) && err.to_string().contains("d2/f1.txt"),
        "expected the missing written blob to be named, got {err}"
    );
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_branch_head("feature").unwrap().unwrap(),
        feature_head
    );
    assert_eq!(repo.get_head().unwrap().unwrap(), feature_head);
    assert!(!temp.path().join("d2/new.txt").exists());
    assert_eq!(read(temp.path(), "d2/f1.txt"), "d2f1\n");
}

/// A conflict the resolver must content-merge needs the base bytes; when
/// they are missing the sync names the conflict and records nothing.
#[tokio::test(flavor = "current_thread")]
async fn sync_conflict_with_missing_base_blob_names_the_conflict() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_blob = repo.put_blob(b"base\n".to_vec()).unwrap();
    let base_entries = with(&wide(&repo), "src/lib/a.rs", &base_blob);
    let base = commit(&repo, "dev", None, base_entries.clone());
    let ours = repo.put_blob(b"ours\n".to_vec()).unwrap();
    let feature_head = commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "src/lib/a.rs", &ours),
    );
    let theirs = repo.put_blob(b"theirs\n".to_vec()).unwrap();
    commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "src/lib/a.rs", &theirs),
    );
    check_out_feature(temp.path(), &repo);
    delete_rows(temp.path(), "blobs", &[base_blob]);
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(
        matches!(err, OakError::IncompleteBlobData { .. })
            && err
                .to_string()
                .contains("conflict resolution for 'src/lib/a.rs'"),
        "got {err}"
    );
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_branch_head("feature").unwrap().unwrap(),
        feature_head
    );
    assert!(!temp.path().join(".oak/SYNC_HEAD").exists());
    assert!(!temp.path().join(".oak/SYNC_STATE").exists());
    assert_eq!(read(temp.path(), "src/lib/a.rs"), "ours\n");
}

/// Both sides changed `d1`, so the merge must read d1's tree on every side;
/// one missing is incomplete manifest data naming that tree.
#[tokio::test(flavor = "current_thread")]
async fn sync_reports_a_missing_tree_it_needs() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());
    let ours = repo.put_blob(b"ours\n".to_vec()).unwrap();
    let feature_head = commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "d1/f0.txt", &ours),
    );
    let theirs = repo.put_blob(b"theirs\n".to_vec()).unwrap();
    let dev_head = commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "d1/f1.txt", &theirs),
    );
    check_out_feature(temp.path(), &repo);
    let dev_d1 = subtree(&repo, &root_of(&repo, &dev_head), "d1");
    delete_rows(temp.path(), "trees", std::slice::from_ref(&dev_d1));
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            OakError::IncompleteManifestData { ref left, ref right, ref missing }
                if left == "feature" && right == "dev" && missing.contains(dev_d1.short())
        ),
        "got {err:?}"
    );
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_branch_head("feature").unwrap().unwrap(),
        feature_head
    );
}

/// `oak merge`'s dirty guard proves a clean tree by tree hash, so the merge
/// runs when HEAD's untouched directories are missing locally: `d5`'s tree
/// and the blobs of `d5` and `d6`. `oak status` keeps its stricter check
/// (every in-cone HEAD blob local) and still refuses that snapshot.
#[tokio::test(flavor = "current_thread")]
async fn local_merge_guard_proves_clean_without_reading_head() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());
    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    let feature_entries = with(&base_entries, "d1/f0.txt", &branch_blob);
    let feature_head = commit(&repo, "feature", Some(&base), feature_entries.clone());
    let parent_blob = repo.put_blob(b"parent\n".to_vec()).unwrap();
    let dev_head = commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "d7/f3.txt", &parent_blob),
    );
    check_out_feature(temp.path(), &repo);
    let d5 = subtree(&repo, &root_of(&repo, &feature_head), "d5");
    let untouched: Vec<Hash> = base_entries
        .iter()
        .filter(|e| e.path.starts_with("d5/") || e.path.starts_with("d6/"))
        .map(|e| e.blob_hash.clone())
        .collect();
    delete_rows(temp.path(), "trees", std::slice::from_ref(&d5));
    delete_rows(temp.path(), "blobs", &untouched);
    drop(repo);

    oak_cli::commands::commit::get_status(temp.path())
        .expect_err("status still requires HEAD complete locally");

    oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false)
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head("dev").unwrap().unwrap();
    let merged = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(merged.parent_hash.as_ref(), Some(&dev_head));
    assert_eq!(merged.merge_parent_hash.as_ref(), Some(&feature_head));
    let expected = with(&feature_entries, "d7/f3.txt", &parent_blob);
    assert_eq!(
        merged.manifest_hash,
        oak_core::build_tree(&expected).unwrap().root_hash
    );
    assert_eq!(read(temp.path(), "d7/f3.txt"), "parent\n");
    assert_eq!(read(temp.path(), "d5/f0.txt"), "d5f0\n");
    assert!(repo.get_tree(&d5).unwrap().is_none(), "nothing read d5");
}

/// The guard's tree proof only ever answers "clean"; a difference falls
/// through to the status comparison, so an untracked file or an edit still
/// refuses the merge before anything moves.
#[tokio::test(flavor = "current_thread")]
async fn local_merge_guard_refuses_untracked_and_edited_files() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base_entries = wide(&repo);
    let base = commit(&repo, "dev", None, base_entries.clone());
    let branch_blob = repo.put_blob(b"branch\n".to_vec()).unwrap();
    let feature_head = commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "d1/f0.txt", &branch_blob),
    );
    check_out_feature(temp.path(), &repo);
    drop(repo);

    for (path, content) in [("scratch.txt", "untracked\n"), ("d3/f1.txt", "edited\n")] {
        fs::write(temp.path().join(path), content).unwrap();
        let err =
            oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false)
                .await
                .expect_err("a dirty tree refuses the merge");
        assert!(matches!(err, OakError::DirtyWorkingTree(_)), "got {err:?}");
        let repo = open_repo(temp.path());
        assert_eq!(
            repo.get_branch_head("feature").unwrap(),
            Some(feature_head.clone())
        );
        assert_eq!(repo.get_branch_head("dev").unwrap(), Some(base.clone()));
        assert_eq!(read(temp.path(), path), content);
        if path == "scratch.txt" {
            fs::remove_file(temp.path().join(path)).unwrap();
        }
    }
}

/// In a sparse checkout the clean check reads only HEAD's directories on
/// the way to the cone (here the root and `src`): the out-of-cone `docs`
/// tree and every out-of-cone blob can be missing, and out-of-cone files on
/// disk are ignored, as `oak status` ignores them. A difference falls
/// through to the status comparison, which reads HEAD whole — it reports the
/// missing tree, then, with the tree back, in-cone edits as dirty.
#[test]
fn clean_check_in_a_sparse_cone_reads_only_toward_the_cone() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    repo.set_metadata(MetadataKey::SparsePaths, "src/lib")
        .unwrap();
    let mut entries = Vec::new();
    for path in [
        "src/lib/a.rs",
        "src/lib/b.rs",
        "src/main.rs",
        "docs/x.md",
        "docs/y/z.md",
        "README.md",
    ] {
        let blob = repo.put_blob(format!("{path}\n").into_bytes()).unwrap();
        entries.push(entry(path, &blob));
    }
    let head = commit(&repo, "feature", None, entries.clone());
    check_out_feature(temp.path(), &repo);
    assert!(!temp.path().join("docs").exists());
    let docs = subtree(&repo, &root_of(&repo, &head), "docs");
    let docs_tree = repo.get_tree(&docs).unwrap().unwrap();
    let out_of_cone: Vec<Hash> = entries
        .iter()
        .filter(|e| !e.path.starts_with("src/lib/"))
        .map(|e| e.blob_hash.clone())
        .collect();
    delete_rows(temp.path(), "trees", std::slice::from_ref(&docs));
    delete_rows(temp.path(), "blobs", &out_of_cone);

    let clean = |repo: &SqliteRepository| {
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(repo, temp.path())
    };
    assert!(clean(&repo).unwrap());
    fs::create_dir_all(temp.path().join("docs")).unwrap();
    fs::write(temp.path().join("docs/new.md"), "outside the cone\n").unwrap();
    assert!(clean(&repo).unwrap(), "out-of-cone files are out of scope");

    fs::write(temp.path().join("src/lib/a.rs"), "edited\n").unwrap();
    assert!(
        matches!(clean(&repo), Err(OakError::ManifestNotFound(_))),
        "the status comparison reads HEAD whole"
    );

    repo.store_tree(&docs_tree).unwrap();
    assert!(!clean(&repo).unwrap());
    fs::write(temp.path().join("src/lib/a.rs"), "src/lib/a.rs\n").unwrap();
    assert!(clean(&repo).unwrap(), "restored content is clean again");
    fs::write(temp.path().join("src/lib/new.rs"), "new\n").unwrap();
    assert!(!clean(&repo).unwrap());
    fs::remove_file(temp.path().join("src/lib/new.rs")).unwrap();
    fs::remove_file(temp.path().join("src/lib/b.rs")).unwrap();
    assert!(!clean(&repo).unwrap(), "a deleted in-cone file is dirty");
}

/// A sparse sync that pauses on an in-cone conflict must be continuable even
/// though out-of-cone bytes were never downloaded: the `--continue` deletion
/// pass compares hashes only, so it scopes its completeness check to the cone
/// (it used to demand every out-of-cone blob and fail as incomplete data).
#[tokio::test(flavor = "current_thread")]
async fn sparse_sync_conflict_continues_without_out_of_cone_bytes() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    repo.set_metadata(MetadataKey::SparsePaths, "src").unwrap();
    let app = repo.put_blob(b"a\nb\nc\n".to_vec()).unwrap();
    let x = repo.put_blob(b"x docs\n".to_vec()).unwrap();
    let base_entries = vec![entry("src/app.rs", &app), entry("docs/x.md", &x)];
    let base = commit(&repo, "dev", None, base_entries.clone());
    let ours = repo.put_blob(b"a\nB1\nc\n".to_vec()).unwrap();
    commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "src/app.rs", &ours),
    );
    let theirs = repo.put_blob(b"a\nB2\nc\n".to_vec()).unwrap();
    let dev_head = commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "src/app.rs", &theirs),
    );
    check_out_feature(temp.path(), &repo);
    delete_rows(temp.path(), "blobs", std::slice::from_ref(&x));
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");
    fs::write(temp.path().join("src/app.rs"), "a\nB\nc\n").unwrap();
    oak_cli::commands::sync::sync_continue(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head("feature").unwrap().unwrap();
    let resolved = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(resolved.merge_parent_hash.as_ref(), Some(&dev_head));
    let files = files_at(&repo, &resolved.manifest_hash);
    assert_eq!(files["docs/x.md"], x, "out-of-cone entry carried forward");
    assert!(!temp.path().join("docs").exists());
}

/// A conflict on a file whose content the server withholds can't be merged
/// locally; the error says so with the access hint instead of reporting
/// "incomplete blob data" (which invites a pointless repair).
#[tokio::test(flavor = "current_thread")]
async fn sync_conflict_on_restricted_content_names_the_access_problem() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let secret = repo.put_blob(b"s\n".to_vec()).unwrap();
    let base_entries = vec![entry("secret/s.txt", &secret)];
    let base = commit(&repo, "dev", None, base_entries.clone());
    let ours = oak_core::hash_bytes(b"ours\n");
    commit(
        &repo,
        "feature",
        Some(&base),
        with(&base_entries, "secret/s.txt", &ours),
    );
    let theirs = oak_core::hash_bytes(b"theirs\n");
    commit(
        &repo,
        "dev",
        Some(&base),
        with(&base_entries, "secret/s.txt", &theirs),
    );
    oak_cli::commands::restricted::record_restricted_blobs(
        &repo,
        &[ours.to_string(), theirs.to_string(), secret.to_string()],
    )
    .unwrap();
    check_out_feature(temp.path(), &repo);
    delete_rows(temp.path(), "blobs", std::slice::from_ref(&secret));
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::RestrictedContent(_)), "got {err:?}");
    assert!(err.to_string().contains("secret/s.txt"), "{err}");
    assert!(err.to_string().contains("ask an org admin"), "{err}");
}
