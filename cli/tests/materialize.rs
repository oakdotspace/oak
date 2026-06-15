//! Regression tests for the consolidated working-tree materializer
//! (`cli/src/materialize.rs`) and the four data-loss bugs it fixes:
//!
//! 1. `oak pull` fast-forward never deleted files removed upstream (and the
//!    deletion then resurrected on the next unattended `oak commit`).
//! 2. `oak pull`'s sync phase re-implemented the manifest merge keyed on
//!    blob hash only, resetting executable bits the real 3-way merge keeps.
//! 3. reset/restore classified entries with `path.is_dir()` (follows
//!    symlinks), so a working-tree symlink to an external directory got
//!    recursed into and the *target's* files deleted.
//! 4. `oak merge` had no preconditions: it merged a stale server-side tip
//!    when the branch wasn't pushed and wiped uncommitted work.

use std::fs;
use std::path::Path;

use oak_core::{FileMode, Hash, ManifestEntry, MetadataKey};
use oak_core::{Repository, SqliteRepository};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DEFAULT_BRANCH: &str = "tester";

/// Minimal repo bootstrap mirroring `integration.rs`'s helper: a
/// deterministic personal branch parented onto `main`.
fn init_repo(dir: &Path) {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoName, "materialize-test")
        .unwrap();
    let br = oak_core::Branch::new(DEFAULT_BRANCH.to_string(), None, Some("main".to_string()));
    repo.store_branch(&br).unwrap();
    repo.set_current_branch(DEFAULT_BRANCH).unwrap();
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

fn head_manifest(repo: &SqliteRepository, branch: &str) -> oak_core::Manifest {
    let head = repo.get_branch_head(branch).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    repo.get_manifest(&commit.manifest_hash).unwrap().unwrap()
}

// ---------------------------------------------------------------------------
// Bug 1: pull fast-forward applies upstream deletions
// ---------------------------------------------------------------------------

/// A pulled commit that deletes a file must remove it from disk and from the
/// stat cache, and the next (unattended) `oak commit` must NOT resurrect it.
#[tokio::test(flavor = "current_thread")]
async fn pull_fast_forward_applies_upstream_deletion() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "keep.txt", "keep\n");
    write_file(temp.path(), "doomed.txt", "doomed\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    let old_head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();

    // The upstream commit's manifest: keep.txt only — doomed.txt deleted.
    let keep_entry = head_manifest(&repo, DEFAULT_BRANCH)
        .entries
        .into_iter()
        .find(|e| e.path == "keep.txt")
        .unwrap();
    let new_manifest_hash = repo.put_manifest(vec![keep_entry]).unwrap();
    let new_head = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // Serve the upstream commit. Blobs/trees ship empty: the blob is already
    // local (shared with keep.txt's current version) and the manifest was
    // stored above, which is exactly the fast-forward shape.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": new_head,
            "branch": null,
            "branches": [{
                "name": DEFAULT_BRANCH,
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": chrono::Utc::now().to_rfc3339(),
            }],
            "commits": [{
                "hash": new_head,
                "branch_name": DEFAULT_BRANCH,
                "parent_hash": old_head.to_string(),
                "manifest_hash": new_manifest_hash.to_string(),
                "author": "peer",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "files": [],
            }],
            "blobs": [],
            "trees": [],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some(DEFAULT_BRANCH),
        Some(&old_head),
        false,
        temp.path(),
        None,
    )
    .await
    .unwrap();
    drop(lock);

    assert!(
        !temp.path().join("doomed.txt").exists(),
        "file deleted upstream must be deleted by the pull fast-forward"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert!(
        !repo.load_stat_cache().unwrap().contains_key("doomed.txt"),
        "stat-cache row for the deleted path must be pruned"
    );

    // The resurrection check: an unattended `oak commit` right after the
    // pull must be a no-op, not a commit that re-adds doomed.txt.
    let head_after_pull = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(head_after_pull.to_string(), new_head);
    drop(repo);
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap(),
        head_after_pull,
        "commit after pull must not resurrect the upstream deletion"
    );
}

// ---------------------------------------------------------------------------
// Bug 2: the pull/sync path keeps executable-bit changes from the parent
// ---------------------------------------------------------------------------

/// The parent branch flips a file's executable bit with no content change.
/// The sync phase of `oak pull` must carry the flip through — the hand-rolled
/// merge it used to have compared blob hashes only and dropped it.
#[tokio::test(flavor = "current_thread")]
async fn sync_from_parent_carries_executable_bit_flip() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // Local (non-main) parent so the sync stays offline.
    repo.store_branch(&oak_core::Branch::new("dev".to_string(), None, None))
        .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("dev".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("feature").unwrap();

    let blob = repo.put_blob(b"echo hi\n".to_vec()).unwrap();
    let base_manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "script.sh".to_string(),
            blob_hash: blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let base = repo
        .put_commit(
            "dev".to_string(),
            None,
            None,
            base_manifest_hash,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("dev", &base).unwrap();
    repo.set_branch_head("feature", &base).unwrap();
    repo.set_head(&base).unwrap();

    // Materialize the base tree on the feature branch.
    {
        let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
        let manifest = head_manifest(&repo, "feature");
        oak_cli::commands::switch::update_working_dir(&lock, temp.path(), &repo, &manifest)
            .unwrap();
    }

    // Parent flips the executable bit, content unchanged.
    let exec_manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "script.sh".to_string(),
            blob_hash: blob,
            mode: FileMode::Executable,
        }])
        .unwrap();
    let dev_head = repo
        .put_commit(
            "dev".to_string(),
            Some(base.clone()),
            None,
            exec_manifest_hash,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("dev", &dev_head).unwrap();
    drop(repo);

    oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    let merged = head_manifest(&repo, "feature");
    let entry = merged
        .entries
        .iter()
        .find(|e| e.path == "script.sh")
        .expect("script.sh survives the sync");
    assert_eq!(
        entry.mode,
        FileMode::Executable,
        "the parent's chmod must survive the sync merge"
    );
    assert_eq!(
        oak_cli::file_permissions::current_file_mode(&temp.path().join("script.sh")),
        Some(FileMode::Executable),
        "the synced working tree must carry the executable bit"
    );
}

// ---------------------------------------------------------------------------
// Bug 3: reset/restore must not follow symlinks into external directories
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn setup_repo_with_external_symlink() -> (TempDir, TempDir) {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "tracked.txt", "tracked\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let external = TempDir::new().unwrap();
    write_file(external.path(), "victim.txt", "do not delete\n");
    std::os::unix::fs::symlink(external.path(), temp.path().join("link")).unwrap();
    (temp, external)
}

/// An untracked working-tree symlink pointing at a directory outside the
/// repo: `oak reset --force` must delete the symlink itself (it's untracked)
/// without recursing through it into the external directory.
#[cfg(unix)]
#[test]
fn reset_deletes_symlink_not_its_external_target() {
    let (temp, external) = setup_repo_with_external_symlink();

    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();

    assert!(
        external.path().join("victim.txt").exists(),
        "reset must never delete files behind a symlink, outside the repo"
    );
    assert!(
        fs::symlink_metadata(temp.path().join("link")).is_err(),
        "the untracked symlink itself is deleted"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "tracked\n"
    );
}

/// Same scenario through `oak restore` (restore-everything form).
#[cfg(unix)]
#[test]
fn restore_deletes_symlink_not_its_external_target() {
    let (temp, external) = setup_repo_with_external_symlink();

    oak_cli::commands::restore::run(temp.path(), &[], None, true).unwrap();

    assert!(
        external.path().join("victim.txt").exists(),
        "restore must never delete files behind a symlink, outside the repo"
    );
    assert!(
        fs::symlink_metadata(temp.path().join("link")).is_err(),
        "the untracked symlink itself is deleted"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "tracked\n"
    );
}

// ---------------------------------------------------------------------------
// Bug 4: oak merge preconditions
// ---------------------------------------------------------------------------

/// `oak merge` on a dirty working tree must refuse before touching anything —
/// both merge paths end in a whole-tree reset that would wipe the edits.
#[tokio::test]
async fn merge_refuses_dirty_working_tree() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "v1\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    write_file(temp.path(), "file.txt", "uncommitted edit\n");

    let err = oak_cli::commands::merge::run(temp.path(), false, false)
        .await
        .expect_err("merge over uncommitted changes must refuse");
    assert!(
        err.to_string().contains("uncommitted"),
        "error should tell the user to commit or reset first, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "uncommitted edit\n",
        "the refused merge must leave the working tree untouched"
    );
}

/// `oak commit && oak merge` without an `oak push` must push the branch to
/// the server before requesting the squash-merge, so the server never merges
/// a stale tip. The push mock's `.expect(1)` is the regression assertion —
/// before the fix, `oak merge` issued no push request at all.
#[tokio::test(flavor = "current_thread")]
async fn merge_pushes_branch_before_server_merge() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let server = MockServer::start().await;
    let repo = open_repo(temp.path());
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    let branch_head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let manifest_hash = repo
        .get_commit(&branch_head)
        .unwrap()
        .unwrap()
        .manifest_hash;
    drop(repo);

    // Push leg: the unpushed commit goes up before the merge is requested.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "head": null })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{DEFAULT_BRANCH}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "head": null })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "missing": [] })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": null,
            "message": "ok",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Merge leg: the server squash-merges and reports the squash commit's
    // shape; its manifest equals the branch tip's (clean merge), which is
    // already in local storage, so no follow-up fetch happens.
    let squash_hash = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/oak/oak/branches/{DEFAULT_BRANCH}/merge"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "squash-merged",
            "commit_hash": squash_hash,
            "manifest_hash": manifest_hash.to_string(),
            "parent_hash": null,
            "merge_parent_hash": branch_head.to_string(),
        })))
        .expect(1)
        .mount(&server)
        .await;

    oak_cli::commands::merge::run(temp.path(), false, false)
        .await
        .unwrap();

    let repo = open_repo(temp.path());
    assert!(
        repo.get_branch(DEFAULT_BRANCH).unwrap().is_none(),
        "merged branch is removed from local history"
    );
    assert_eq!(
        repo.get_branch_head("main").unwrap().unwrap(),
        Hash(squash_hash.to_string()),
        "local main mirrors the server's squash commit"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "content\n",
        "the merged working tree keeps the (pushed) content"
    );
}

// ---------------------------------------------------------------------------
// Bug 5: a conflicted parent-sync must not sweep unrelated dirty files into
// the sync commit (`oak pull --continue` used to snapshot the whole tree)
// ---------------------------------------------------------------------------

/// Shared setup: feature branch parented on a local `dev` branch, both forked
/// from a base commit with four files. The parent then edits `conflict.txt`
/// (which the feature branch also edited — the conflict), cleanly edits
/// `clean.txt`, and deletes `gone.txt`; `mine.txt` is untouched.
/// Returns the temp dir with the sync paused on the conflict.
async fn setup_conflicted_sync() -> TempDir {
    use oak_core::FileMode;

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    // Local (non-main) parent so the sync stays offline.
    repo.store_branch(&oak_core::Branch::new("dev".to_string(), None, None))
        .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("dev".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("feature").unwrap();

    let entry = |path: &str, blob: &Hash| ManifestEntry {
        path: path.to_string(),
        blob_hash: blob.clone(),
        mode: FileMode::Regular,
    };

    let base_conflict = repo.put_blob(b"base\n".to_vec()).unwrap();
    let old_clean = repo.put_blob(b"old\n".to_vec()).unwrap();
    let bye = repo.put_blob(b"bye\n".to_vec()).unwrap();
    let tracked = repo.put_blob(b"tracked\n".to_vec()).unwrap();

    let base_manifest_hash = repo
        .put_manifest(vec![
            entry("conflict.txt", &base_conflict),
            entry("clean.txt", &old_clean),
            entry("gone.txt", &bye),
            entry("mine.txt", &tracked),
        ])
        .unwrap();
    let base = repo
        .put_commit(
            "dev".to_string(),
            None,
            None,
            base_manifest_hash,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("dev", &base).unwrap();
    repo.set_branch_head("feature", &base).unwrap();
    repo.set_head(&base).unwrap();

    // The feature branch edits conflict.txt its own way.
    let feature_conflict = repo.put_blob(b"feature\n".to_vec()).unwrap();
    let feature_manifest_hash = repo
        .put_manifest(vec![
            entry("conflict.txt", &feature_conflict),
            entry("clean.txt", &old_clean),
            entry("gone.txt", &bye),
            entry("mine.txt", &tracked),
        ])
        .unwrap();
    let feature_head = repo
        .put_commit(
            "feature".to_string(),
            Some(base.clone()),
            None,
            feature_manifest_hash,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("feature", &feature_head).unwrap();
    repo.set_head(&feature_head).unwrap();

    // Materialize the feature tip.
    {
        let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
        let manifest = head_manifest(&repo, "feature");
        oak_cli::commands::switch::update_working_dir(&lock, temp.path(), &repo, &manifest)
            .unwrap();
    }

    // The parent edits conflict.txt differently, edits clean.txt cleanly,
    // and deletes gone.txt.
    let parent_conflict = repo.put_blob(b"parent\n".to_vec()).unwrap();
    let new_clean = repo.put_blob(b"new\n".to_vec()).unwrap();
    let dev_manifest_hash = repo
        .put_manifest(vec![
            entry("conflict.txt", &parent_conflict),
            entry("clean.txt", &new_clean),
            entry("mine.txt", &tracked),
        ])
        .unwrap();
    let dev_head = repo
        .put_commit(
            "dev".to_string(),
            Some(base),
            None,
            dev_manifest_hash,
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("dev", &dev_head).unwrap();
    drop(repo);

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(
        matches!(err, oak_core::OakError::MergeConflict(1)),
        "expected exactly the conflict.txt conflict, got: {err}"
    );
    assert!(
        temp.path().join(".oak/SYNC_STATE").exists(),
        "a conflicted sync records SYNC_STATE for the scoped continue"
    );

    temp
}

/// `oak pull --continue` commits the merge result plus the user's conflict
/// resolutions — NOT the unrelated dirty files made while resolving, and not
/// files the parent deleted that are still sitting on disk.
#[tokio::test(flavor = "current_thread")]
async fn sync_continue_scopes_commit_to_merge_and_resolutions() {
    let temp = setup_conflicted_sync().await;

    // While resolving, the user also dirties an unrelated tracked file and
    // drops an untracked scratch file.
    write_file(temp.path(), "mine.txt", "dirty edit\n");
    write_file(temp.path(), "scratch.txt", "scratch\n");
    // Resolve the conflict.
    write_file(temp.path(), "conflict.txt", "resolved\n");

    oak_cli::commands::sync::sync_continue(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let manifest = head_manifest(&repo, "feature");
    let by_path = |p: &str| manifest.entries.iter().find(|e| e.path == p);

    let conflict = by_path("conflict.txt").expect("resolution committed");
    assert_eq!(
        repo.get_blob(&conflict.blob_hash).unwrap().unwrap().content,
        b"resolved\n",
        "the sync commit carries the user's resolution"
    );
    let clean = by_path("clean.txt").expect("parent's clean edit committed");
    assert_eq!(
        repo.get_blob(&clean.blob_hash).unwrap().unwrap().content,
        b"new\n",
    );
    let mine = by_path("mine.txt").expect("mine.txt stays tracked");
    assert_eq!(
        repo.get_blob(&mine.blob_hash).unwrap().unwrap().content,
        b"tracked\n",
        "the unrelated dirty edit must NOT be committed by the sync"
    );
    assert!(
        by_path("scratch.txt").is_none(),
        "untracked scratch files must NOT be swept into the sync commit"
    );
    assert!(
        by_path("gone.txt").is_none(),
        "the parent's deletion lands in the sync commit"
    );

    // Working tree: dirty files preserved, parent deletion applied.
    assert_eq!(
        fs::read_to_string(temp.path().join("mine.txt")).unwrap(),
        "dirty edit\n",
        "the dirty edit stays on disk as an uncommitted change"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("scratch.txt")).unwrap(),
        "scratch\n",
        "the scratch file stays on disk"
    );
    assert!(
        !temp.path().join("gone.txt").exists(),
        "a file the parent deleted (and the user didn't touch) is removed"
    );
    assert!(
        !temp.path().join(".oak/SYNC_STATE").exists(),
        "SYNC_STATE is cleaned up"
    );
    assert!(
        !repo.load_stat_cache().unwrap().contains_key("gone.txt"),
        "stat-cache row for the deleted path is pruned"
    );

    // The dirty files surface on the next status/commit instead of vanishing.
    let (changes, _, _) = oak_cli::commands::commit::compute_changes(&repo, temp.path()).unwrap();
    let changed: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert!(changed.contains(&"mine.txt"), "mine.txt shows as dirty");
    assert!(changed.contains(&"scratch.txt"), "scratch.txt shows as new");
    assert_eq!(changed.len(), 2, "nothing else is dirty: {changed:?}");
}

/// A sync paused by an older binary has no SYNC_STATE; `--continue` falls
/// back to the legacy whole-tree snapshot rather than failing.
#[tokio::test(flavor = "current_thread")]
async fn sync_continue_without_sync_state_falls_back_to_snapshot() {
    let temp = setup_conflicted_sync().await;
    fs::remove_file(temp.path().join(".oak/SYNC_STATE")).unwrap();

    write_file(temp.path(), "scratch.txt", "scratch\n");
    write_file(temp.path(), "conflict.txt", "resolved\n");

    oak_cli::commands::sync::sync_continue(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let manifest = head_manifest(&repo, "feature");
    assert!(
        manifest.entries.iter().any(|e| e.path == "scratch.txt"),
        "legacy fallback keeps the old whole-tree snapshot behavior"
    );
    assert!(
        manifest.entries.iter().any(|e| e.path == "conflict.txt"),
        "resolution is committed"
    );
}
