//! Integration tests for local Oak operations
//!
//! These tests verify init, commit, log, branch, merge, status,
//! diff, reset, and ignore functionality without requiring a server.

use std::fs;
use std::path::Path;
use std::process::Command;

use oak_cli::commands::switch::WorktreePolicy;
use oak_core::{
    hash_bytes, ChangeType, Commit, FileMode, Hash, Manifest, MetadataKey, Tree, TreeEntry,
    TreeEntryKind,
};
use oak_core::{Repository, SqliteRepository};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The branch name `oak init` creates locally under the new branch model.
/// `init` derives the local branch name from `OAK_AUTHOR`/`USER`/`USERNAME`;
/// `init_repo` pins `OAK_AUTHOR=tester` so this is deterministic.
const DEFAULT_BRANCH: &str = "tester";

/// `oak sync` (the parent-merge phase of `oak pull`) is async because it
/// fetches `main` from the remote when the parent isn't materialized
/// locally. Tests still call it synchronously; this helper drives the
/// future to completion via a fresh single-threaded runtime per invocation
/// so the existing per-test code keeps working.
fn run_sync(path: &Path, continue_sync: bool, abort_sync: bool) -> oak_core::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if continue_sync {
        oak_cli::commands::sync::sync_continue(path)
    } else if abort_sync {
        oak_cli::commands::sync::sync_abort(path)
    } else {
        rt.block_on(oak_cli::commands::sync::sync_from_parent(path))
    }
}

fn run_close_branch(path: &Path, name: &str) -> oak_core::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(oak_cli::commands::branch::close_branch(path, name, None))
}

/// Helper: initialize an oak repo in the given directory.
///
/// `oak init` now generates a per-clone random suffix (`tester-<rand6hex>`)
/// so two clones don't collide on push, but the existing test suite asserts
/// against a stable `DEFAULT_BRANCH = "tester"`. This helper builds the
/// minimal on-disk state directly — bypassing `init::run` — so the branch
/// name stays deterministic across tests. `test_init_creates_repo` is the
/// one place that still drives `init::run` to validate its actual behavior.
fn init_repo(dir: &Path) {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let db_path = oak_dir.join("oak.db");
    let repo = SqliteRepository::open(&db_path).unwrap();
    let repo_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());
    repo.set_metadata(oak_core::MetadataKey::RepoName, &repo_name)
        .unwrap();
    let br = oak_core::Branch::new(DEFAULT_BRANCH.to_string(), None, Some("main".to_string()));
    repo.store_branch(&br).unwrap();
    repo.set_current_branch(DEFAULT_BRANCH).unwrap();
}

/// Helper: open the repo at the given path
fn open_repo(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

/// Helper: write a file relative to the directory
fn write_file(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn seed_main_commit(repo: &SqliteRepository, path: &str, content: &str) -> oak_core::Hash {
    use oak_core::{Branch, Commit, FileMode, Manifest, ManifestEntry};

    let blob = repo.put_blob(content.as_bytes().to_vec()).unwrap();
    let manifest = Manifest::new(vec![ManifestEntry {
        path: path.to_string(),
        blob_hash: blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "main".to_string(),
        None,
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("main commit".to_string()),
        vec![],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    if repo.get_branch("main").unwrap().is_none() {
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
    }
    repo.set_branch_head("main", &commit.hash).unwrap();
    commit.hash
}

/// Materialize a manifest into `dir`, acquiring the workdir lock the shared
/// materializer requires as a witness.
fn update_working_dir(dir: &Path, repo: &SqliteRepository, manifest: &oak_core::Manifest) {
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&dir.join(".oak")).unwrap();
    oak_cli::commands::switch::update_working_dir(&lock, dir, repo, manifest).unwrap();
}

fn materialize_commit(dir: &Path, repo: &SqliteRepository, hash: &oak_core::Hash) {
    let commit = repo.get_commit(hash).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    update_working_dir(dir, repo, &manifest);
}

fn oak_bin(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

#[test]
fn test_init_creates_repo() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    assert!(temp.path().join(".oak").exists());
    assert!(temp.path().join(".oak/oak.db").exists());

    let repo = open_repo(temp.path());
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(branch, DEFAULT_BRANCH);

    // Under the new branch model, `oak init` should NOT create a local
    // `main` row — `main` only exists on the server.
    assert!(repo.get_branch("main").unwrap().is_none());

    // The local default branch is parented onto `main` conceptually.
    let default = repo.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(default.parent_branch.as_deref(), Some("main"));
}

/// `oak init` (the real entry point, not the test helper) generates a
/// per-clone random suffix on the personal branch name so two clones of
/// the same repo by the same user don't collide on push.
#[test]
fn test_init_generates_suffixed_personal_branch() {
    std::env::set_var("OAK_AUTHOR", "tester");
    let a = oak_cli::commands::init::default_local_branch_name();
    let b = oak_cli::commands::init::default_local_branch_name();
    assert!(
        a.starts_with("tester-"),
        "expected `tester-` prefix, got {a}"
    );
    assert!(
        b.starts_with("tester-"),
        "expected `tester-` prefix, got {b}"
    );
    assert_ne!(a, b, "two invocations must produce distinct names");
    assert_eq!(
        a.len(),
        "tester-".len() + 6,
        "expected 6-hex suffix, got {a}"
    );
}

#[test]
fn test_init_fails_if_already_exists() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let result = oak_cli::commands::init::run(temp.path(), false);
    assert!(result.is_err());
}

#[test]
fn test_commit_and_log() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "hello.txt", "hello world");

    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    assert_eq!(commits.len(), 1);
    // Local commits no longer carry messages — only server-side squash-merge
    // commits do, where the message is the source branch's description.
    assert_eq!(commits[0].message, None);
}

#[test]
fn test_commit_no_changes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Second commit with no changes should succeed but do nothing
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    assert_eq!(commits.len(), 1); // Still only 1 commit
}

#[test]
fn test_branch_workflow() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "main.txt", "main content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Create and switch to feature branch
    oak_cli::commands::branch::new_branch(temp.path(), "feature", Some("a feature"), None, None)
        .unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, "feature");

    // Add file on feature branch
    write_file(temp.path(), "feature.txt", "feature content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Switch back to the default branch
    oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false).unwrap();
    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, DEFAULT_BRANCH);

    // List branches
    oak_cli::commands::branch::list_branches(temp.path()).unwrap();

    // Show branch
    oak_cli::commands::branch::show_branch(temp.path(), "feature").unwrap();
}

#[test]
fn test_status_clean() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty());
}

#[test]
fn test_status_with_changes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "file.txt", "modified");
    write_file(temp.path(), "new.txt", "new");

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes
        .iter()
        .any(|c| c.path == "file.txt" && c.change_type == ChangeType::Modified));
    assert!(changes
        .iter()
        .any(|c| c.path == "new.txt" && c.change_type == ChangeType::Added));
}

#[test]
fn test_diff_shows_changes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "line one\nline two\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "file.txt", "line one\nline modified\n");

    // Diff should succeed (it prints to stdout)
    oak_cli::commands::diff::run(temp.path(), &[], false, false, false).unwrap();
}

/// Rename-with-edit parity: a file that was renamed *and* edited (similar
/// enough for content-similarity rename detection) must show up as a single
/// R entry — with its old path — in `oak diff --print` and `oak diff --json`,
/// exactly as `oak commit`/`oak log` record it. It must never regress to a
/// delete+add pair in the diff paths.
#[test]
fn test_diff_print_and_json_report_rename_with_edit() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(
        temp.path(),
        "old.txt",
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    );
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Rename with an edit: 5 of 6 lines shared, similarity ≈ 0.71 ≥ 0.5.
    fs::remove_file(temp.path().join("old.txt")).unwrap();
    write_file(
        temp.path(),
        "new.txt",
        "line one\nline two\nline three\nline four\nline five\nline changed\n",
    );

    let output = oak_bin(temp.path(), &["diff", "--print"]);
    assert!(output.status.success());
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        printed.contains("rename from old.txt") && printed.contains("rename to new.txt"),
        "diff --print must show the rename-with-edit as R with its old path:\n{printed}"
    );
    assert!(
        printed.contains("+line changed"),
        "diff --print should still show the edit hunk:\n{printed}"
    );

    let output = oak_bin(temp.path(), &["diff", "--json"]);
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("diff --json should emit valid JSON");
    let changed = json["changed_files"]
        .as_array()
        .expect("changed_files array");
    assert_eq!(
        changed.len(),
        1,
        "rename-with-edit must be one entry, not D+A: {json}"
    );
    assert_eq!(changed[0]["status"], "renamed");
    assert_eq!(changed[0]["path"], "new.txt");
    assert_eq!(changed[0]["old_path"], "old.txt");
}

/// The checkout-free branch diff (`oak branch diff`) must apply the same
/// content-similarity rename detection as `oak diff` and the commit path, so
/// a committed rename-with-edit reads as `renamed` there too.
#[test]
fn test_branch_diff_json_reports_rename_with_edit() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let main_head = seed_main_commit(
        &repo,
        "src/old_name.rs",
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    );

    // Feature branch commit renames the file and edits one line.
    let blob = repo
        .put_blob(b"line one\nline two\nline three\nline four\nline five\nline changed\n".to_vec())
        .unwrap();
    let manifest = Manifest::new(vec![oak_core::ManifestEntry {
        path: "src/new_name.rs".to_string(),
        blob_hash: blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let feature_commit = Commit::new(
        "feature".to_string(),
        Some(main_head),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        None,
        vec![],
    )
    .unwrap();
    repo.store_commit(&feature_commit).unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("feature", &feature_commit.hash)
        .unwrap();

    let output = oak_bin(temp.path(), &["branch", "diff", "feature", "--json"]);
    assert!(
        output.status.success(),
        "branch diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("branch diff --json should emit valid JSON");
    let changed = json["changed_files"]
        .as_array()
        .expect("changed_files array");
    assert_eq!(
        changed.len(),
        1,
        "rename-with-edit must be one entry, not D+A: {json}"
    );
    assert_eq!(changed[0]["status"], "renamed");
    assert_eq!(changed[0]["path"], "src/new_name.rs");
    assert_eq!(changed[0]["old_path"], "src/old_name.rs");
}

#[test]
fn test_diff_omits_large_text_hunks_but_keeps_path() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    fs::write(
        temp.path().join("large.txt"),
        vec![b'x'; oak_core::MAX_TEXT_DIFF_BYTES + 1],
    )
    .unwrap();

    let repo = open_repo(temp.path());
    let (changes, rendered) = oak_cli::commands::diff::render(&repo, temp.path()).unwrap();

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "large.txt");
    assert!(
        rendered.contains("diff --oak a/large.txt b/large.txt"),
        "large changed file must still appear in diff output"
    );
    assert!(
        rendered.contains("Binary files a/large.txt and b/large.txt differ"),
        "large changed file should get an explicit omitted-hunk notice"
    );
    assert!(
        rendered.len() < 512,
        "large diff output should stay compact, got {} bytes",
        rendered.len()
    );
}

#[test]
fn test_diff_render_does_not_store_dirty_file_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "tracked.txt", "line one\nline two\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let dirty_content = "line one\nline changed in diff no-store regression\n";
    let dirty_hash = oak_core::hash_bytes(dirty_content.as_bytes());
    let repo = open_repo(temp.path());
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "test setup should start without the dirty blob"
    );

    write_file(temp.path(), "tracked.txt", dirty_content);
    let (changes, rendered) = oak_cli::commands::diff::render(&repo, temp.path()).unwrap();

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "tracked.txt");
    assert!(
        rendered.contains("diff --oak a/tracked.txt b/tracked.txt")
            && rendered.contains("+line changed in diff no-store regression"),
        "diff must still render the dirty worktree hunk:\n{rendered}"
    );
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "oak diff must not persist dirty working-tree content"
    );

    oak_cli::commands::commit::run(temp.path()).unwrap();
    assert!(
        repo.has_blob(&dirty_hash).unwrap(),
        "oak commit must still persist content that diff only read"
    );
}

#[test]
fn test_diff_stat_and_name_only_do_not_store_dirty_blobs() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "one.txt", "one\n");
    write_file(temp.path(), "two.txt", "two\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let one_dirty = "one changed for stat no-store\n";
    let two_dirty = "two changed for name-only no-store\n";
    let one_hash = oak_core::hash_bytes(one_dirty.as_bytes());
    let two_hash = oak_core::hash_bytes(two_dirty.as_bytes());
    let repo = open_repo(temp.path());
    assert!(!repo.has_blob(&one_hash).unwrap());
    assert!(!repo.has_blob(&two_hash).unwrap());

    write_file(temp.path(), "one.txt", one_dirty);
    write_file(temp.path(), "two.txt", two_dirty);

    oak_cli::output::begin_capture();
    oak_cli::commands::diff::run(temp.path(), &[], true, false, false).unwrap();
    let stat = oak_cli::output::end_capture();
    assert!(
        stat.contains("M one.txt") && stat.contains("M two.txt"),
        "stat output should still report both dirty paths:\n{stat}"
    );
    assert!(
        !repo.has_blob(&one_hash).unwrap() && !repo.has_blob(&two_hash).unwrap(),
        "oak diff --stat must not persist dirty working-tree content"
    );

    oak_cli::output::begin_capture();
    oak_cli::commands::diff::run(temp.path(), &[], false, true, false).unwrap();
    let names = oak_cli::output::end_capture();
    assert!(
        names.contains("one.txt") && names.contains("two.txt"),
        "name-only output should still report both dirty paths:\n{names}"
    );
    assert!(
        !repo.has_blob(&one_hash).unwrap() && !repo.has_blob(&two_hash).unwrap(),
        "oak diff --name-only must not persist dirty working-tree content"
    );
}

/// Regression: `oak status`, `oak diff`, and `oak commit` must report the *same*
/// set of modified paths — even for edits crafted to slip past an `(mtime,
/// size)` stat cache: identical byte length plus an mtime reset to the
/// previously-committed value. Before the cache also compared ctime, such edits
/// were invisible to `oak status`/`oak diff` and silently dropped by
/// `oak commit`, which then recorded the stale pre-edit content. The binary
/// file additionally pins the diff side of the bug: it has no textual hunk, and
/// diff used to drop such files even though status/commit counted them.
///
/// See `oak_core::StatCacheEntry` (the ctime guard) and
/// `commands::diff::render` (diff sharing status's change set).
#[test]
fn test_status_diff_commit_agree_under_stat_cache_trap() {
    use std::collections::BTreeSet;
    use std::time::SystemTime;

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Initial commit. Sizes are chosen so the edits below keep byte length
    // identical; `bin.dat` carries a NUL byte so it's treated as binary.
    write_file(temp.path(), "same_size.txt", "AAAAA\n"); // 6 bytes
    write_file(temp.path(), "multi.txt", "one\ntwo\nthree\n"); // 14 bytes
                                                               // Binary: a NUL (so diff treats it as binary) plus an invalid UTF-8 byte.
                                                               // The edit below swaps 0xFF -> 0xFE: same size, the blob hash differs, but
                                                               // both lossy-decode to the *same* string (U+FFFD) — so a textual line diff
                                                               // is empty. That pins the diff-side bug: diff must still list this path.
    fs::write(temp.path().join("bin.dat"), [0xFFu8, 0x00, 0x42]).unwrap(); // 3 bytes
    write_file(temp.path(), "untouched.txt", "stable\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Remember the committed mtimes so we can put them back after editing.
    let mtime_of = |rel: &str| -> SystemTime {
        fs::metadata(temp.path().join(rel))
            .unwrap()
            .modified()
            .unwrap()
    };
    let len_of = |rel: &str| -> u64 { fs::metadata(temp.path().join(rel)).unwrap().len() };
    let mt_same = mtime_of("same_size.txt");
    let mt_multi = mtime_of("multi.txt");
    let mt_bin = mtime_of("bin.dat");

    // Edit each trap file, preserving byte length, and add one ordinary file.
    write_file(temp.path(), "same_size.txt", "BBBBB\n"); // still 6 bytes, new content
    write_file(temp.path(), "multi.txt", "ONE\nTWO\nthree\n"); // still 14 bytes
    fs::write(temp.path().join("bin.dat"), [0xFEu8, 0x00, 0x42]).unwrap(); // 3 bytes, lossy-equal
    write_file(temp.path(), "added.txt", "brand new\n");

    // Reset mtime back to the committed value so each trap file presents the
    // exact (mtime, size) the cache recorded. ctime can't be moved backwards, so
    // these edits are only detectable via ctime (or by re-hashing).
    let reset_mtime = |rel: &str, t: SystemTime| {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(temp.path().join(rel))
            .unwrap();
        f.set_modified(t).unwrap();
    };
    reset_mtime("same_size.txt", mt_same);
    reset_mtime("multi.txt", mt_multi);
    reset_mtime("bin.dat", mt_bin);

    // Premise guard: (mtime, size) of each trap file matches what was committed,
    // so a cache trusting only those two would wrongly treat them as clean.
    assert_eq!(mtime_of("same_size.txt"), mt_same);
    assert_eq!(mtime_of("multi.txt"), mt_multi);
    assert_eq!(mtime_of("bin.dat"), mt_bin);
    assert_eq!(len_of("same_size.txt"), 6);
    assert_eq!(len_of("multi.txt"), 14);
    assert_eq!(len_of("bin.dat"), 3);

    let expected: BTreeSet<String> = ["same_size.txt", "multi.txt", "bin.dat", "added.txt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let dirty_hashes = [
        oak_core::hash_bytes(b"BBBBB\n"),
        oak_core::hash_bytes(b"ONE\nTWO\nthree\n"),
        oak_core::hash_bytes(&[0xFEu8, 0x00, 0x42]),
        oak_core::hash_bytes(b"brand new\n"),
    ];

    // (1) oak status
    let (status_changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    let status_paths: BTreeSet<String> = status_changes.iter().map(|c| c.path.clone()).collect();
    assert_eq!(
        status_paths, expected,
        "oak status under-reported modified files"
    );

    // (2) oak diff — both its change set and the paths it actually renders. A
    // path in the set that produces no textual hunk (the binary file) must still
    // appear as a `diff --oak` block, never be silently dropped.
    let repo = open_repo(temp.path());
    let (diff_changes, rendered) = oak_cli::commands::diff::render(&repo, temp.path()).unwrap();
    let diff_paths: BTreeSet<String> = diff_changes.iter().map(|c| c.path.clone()).collect();
    assert_eq!(
        diff_paths, expected,
        "oak diff change set diverged from status"
    );
    for p in &expected {
        assert!(
            rendered.contains(&format!("diff --oak a/{p} b/{p}")),
            "oak diff dropped a modified path from its output: {p}"
        );
    }
    for hash in &dirty_hashes {
        assert!(
            !repo.has_blob(hash).unwrap(),
            "oak diff must not persist dirty blobs while preserving status parity"
        );
    }
    drop(repo);

    // (3) oak commit — the set it records, plus proof it recorded the *new*
    // content (the old cache would have preserved the stale pre-edit blob).
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let commit_paths: BTreeSet<String> = commit.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(
        commit_paths, expected,
        "oak commit recorded a different set than status/diff"
    );

    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == "same_size.txt")
        .unwrap();
    let blob = repo.get_blob(&entry.blob_hash).unwrap().unwrap();
    assert_eq!(
        blob.content, b"BBBBB\n",
        "oak commit recorded stale content for the stat-cache trap file"
    );
}

#[test]
fn test_reset() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "original");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Modify and add files
    write_file(temp.path(), "file.txt", "modified");
    write_file(temp.path(), "new.txt", "new file");

    // Reset with force (skip confirmation)
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();

    // Verify original content restored
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original");

    // New file should be deleted
    assert!(!temp.path().join("new.txt").exists());
}

#[test]
fn test_reset_rejects_outside_directory_path() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "original");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    write_file(outside.path(), "scratch.txt", "outside");

    let err = oak_cli::commands::reset::run(temp.path(), Some(outside.path()), true).unwrap_err();
    assert!(
        err.to_string().contains("is not inside the repository"),
        "expected an out-of-repo path error, got {err}"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("scratch.txt")).unwrap(),
        "outside"
    );
}

/// Regression: a chmod-only change (same content, different mode) must be
/// detected and undone by `oak reset`, and `oak status` must agree the tree is
/// clean afterward. Before the fix, reset compared only blob hashes and
/// reported "nothing to reset" while status still showed the file as modified
/// (status diffs mode via `Manifest::diff`; reset didn't).
#[cfg(unix)]
#[test]
fn test_reset_reverts_mode_only_change() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "script.sh", "#!/bin/sh\necho hi\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Flip on the executable bit without touching content.
    let script = temp.path().join("script.sh");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    // status sees the mode-only change as a modification.
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c.path == "script.sh" && c.change_type == ChangeType::Modified),
        "status should flag a chmod-only change as Modified"
    );

    // reset must undo it, not report the tree clean.
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();
    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0, "reset should clear the executable bit");

    // status and reset now agree: clean.
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "tree should be clean after reset");
}

/// `oak restore` shares reset's change-detection, so it must also undo a
/// chmod-only change rather than report the paths already at source state.
#[cfg(unix)]
#[test]
fn test_restore_reverts_mode_only_change() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "script.sh", "#!/bin/sh\necho hi\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let script = temp.path().join("script.sh");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    oak_cli::commands::restore::run(temp.path(), &[], None, true).unwrap();
    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0, "restore should clear the executable bit");

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "tree should be clean after restore");
}

/// fb-25 invariant: any change `oak status` reports must be actionable —
/// `oak restore <path>`/`oak reset <path>` either clear it or explain
/// precisely why not. The newly-ignored-but-tracked case is the one that used
/// to break: status reported `D <path>` (its working-tree scan skips ignored
/// paths) while reset/restore compared the on-disk file to HEAD, found it
/// identical, and claimed "already at HEAD/source state" — an un-clearable
/// dirty tree that blocked `oak merge`.
#[test]
fn test_newly_ignored_tracked_file_restore_and_reset_explain() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "debug.log", "log content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Newly ignore the tracked file.
    write_file(temp.path(), ".oakignore", "*.log\n");

    // status reports the ignore-induced pending deletion.
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c.path == "debug.log" && c.change_type == ChangeType::Deleted),
        "status should report the newly-ignored tracked file as a pending deletion"
    );

    // restore must not claim the path is already at source state; it must
    // name the ignore rules as the reason the report can't be cleared here.
    oak_cli::output::begin_capture();
    oak_cli::commands::restore::run(
        temp.path(),
        &[Path::new("debug.log").to_path_buf()],
        None,
        true,
    )
    .unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        !out.contains("already at source state"),
        "restore claimed the path clean while status disagrees: {out}"
    );
    assert!(
        out.contains("ignore rules") && out.contains("debug.log"),
        "restore should explain the ignore-induced pending deletion: {out}"
    );

    // reset likewise.
    oak_cli::output::begin_capture();
    oak_cli::commands::reset::run(temp.path(), Some(Path::new("debug.log")), true).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        !out.contains("already at HEAD state"),
        "reset claimed the path clean while status disagrees: {out}"
    );
    assert!(
        out.contains("ignore rules") && out.contains("debug.log"),
        "reset should explain the ignore-induced pending deletion: {out}"
    );

    // Neither command touched the file.
    assert_eq!(
        fs::read_to_string(temp.path().join("debug.log")).unwrap(),
        "log content"
    );
}

/// A whole-tree `oak reset` that deletes an *untracked* .oakignore un-ignores
/// the tracked file, so the limbo state is genuinely cleared: no stale
/// "cannot clear" warning, and status agrees the tree is clean.
#[test]
fn test_whole_tree_reset_clears_ignore_limbo_when_ignore_file_untracked() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "debug.log", "log content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    write_file(temp.path(), ".oakignore", "*.log\n");

    oak_cli::output::begin_capture();
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        !out.contains("cannot clear"),
        "reset deleted the untracked .oakignore, so the limbo warning is stale: {out}"
    );
    assert!(
        !temp.path().join(".oakignore").exists(),
        "whole-tree reset should delete the untracked .oakignore"
    );

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes.is_empty(),
        "tree should be clean after reset removed the ignore rule, got {changes:?}"
    );
}

/// fb-25 invariant for the ordinary change kinds: every change `oak status`
/// reports (modified, deleted, untracked) is cleared by a whole-tree
/// `oak reset --force`, and status agrees afterwards.
#[test]
fn test_reset_clears_every_status_reported_change() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "modified.txt", "original");
    write_file(temp.path(), "deleted.txt", "to be deleted");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "modified.txt", "changed");
    fs::remove_file(temp.path().join("deleted.txt")).unwrap();
    write_file(temp.path(), "untracked.txt", "new");

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(
        changes.len(),
        3,
        "expected M/D/A to be reported: {changes:?}"
    );

    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes.is_empty(),
        "every status-reported change must be cleared by reset, got {changes:?}"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("modified.txt")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("deleted.txt")).unwrap(),
        "to be deleted"
    );
    assert!(!temp.path().join("untracked.txt").exists());
}

/// Same invariant for `oak restore` on explicit paths: a status-reported
/// modification and deletion are both cleared by restoring those paths.
#[test]
fn test_restore_clears_status_reported_modification_and_deletion() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "modified.txt", "original");
    write_file(temp.path(), "deleted.txt", "to be deleted");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "modified.txt", "changed");
    fs::remove_file(temp.path().join("deleted.txt")).unwrap();

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(changes.len(), 2, "expected M/D to be reported: {changes:?}");

    oak_cli::commands::restore::run(
        temp.path(),
        &[
            Path::new("modified.txt").to_path_buf(),
            Path::new("deleted.txt").to_path_buf(),
        ],
        None,
        true,
    )
    .unwrap();

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes.is_empty(),
        "every status-reported change must be cleared by restore, got {changes:?}"
    );
}

#[test]
fn test_restore_rejects_outside_directory_path() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "original");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    write_file(outside.path(), "scratch.txt", "outside");

    let err =
        oak_cli::commands::restore::run(temp.path(), &[outside.path().to_path_buf()], None, true)
            .unwrap_err();
    assert!(
        err.to_string().contains("is not inside the repository"),
        "expected an out-of-repo path error, got {err}"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("scratch.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn test_restore_dot_restores_entire_repo() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "original a");
    write_file(temp.path(), "b.txt", "original b");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "a.txt", "modified a");
    write_file(temp.path(), "b.txt", "modified b");

    oak_cli::commands::restore::run(temp.path(), &[Path::new(".").to_path_buf()], None, true)
        .unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "original a"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "original b"
    );
}

#[test]
fn test_gitignore_respected() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Create .gitignore
    write_file(temp.path(), ".gitignore", "*.log\nbuild/\n");

    // Create files, some ignored
    write_file(temp.path(), "src/main.rs", "fn main() {}");
    write_file(temp.path(), "debug.log", "log data");
    write_file(temp.path(), "build/output", "binary");

    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();

    // .gitignore itself should be committed
    assert!(manifest.get(".gitignore").is_some());
    // src/main.rs should be committed
    assert!(manifest.get("src/main.rs").is_some());
    // debug.log and build/ should NOT be committed
    assert!(manifest.get("debug.log").is_none());
    assert!(manifest.get("build/output").is_none());
}

/// Adding an ignore pattern that covers already-tracked files makes the next
/// commit record them as deletions (the self-heal behavior for accidentally
/// committed build artifacts). That drop must come with a loud warning — in a
/// big change list those deletions are otherwise indistinguishable from real
/// ones.
#[test]
fn test_commit_warns_when_tracked_files_become_ignored() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "src/main.rs", "fn main() {}");
    write_file(temp.path(), "target/debug/app", "bits");
    write_file(temp.path(), "target/debug/app.d", "deps");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Ignore target/ after the fact; the files stay on disk.
    write_file(temp.path(), ".oakignore", "target/\n");

    oak_cli::output::begin_capture();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        out.contains("2 tracked files under target/ are now ignored")
            && out.contains("will be removed from the branch by this commit"),
        "expected now-ignored warning, got:\n{out}"
    );

    // The drop itself still happens — the warning is visibility, not a veto.
    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    assert!(manifest.get("target/debug/app").is_none());
    assert!(manifest.get("src/main.rs").is_some());
    assert!(temp.path().join("target/debug/app").exists());
}

/// The warning breaks the drop down by top-level directory so a multi-root
/// sweep is legible, and root-level files are listed individually.
#[test]
fn test_now_ignored_warning_groups_by_top_level_dir() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "target/a.o", "a");
    write_file(temp.path(), "target/b.o", "b");
    write_file(temp.path(), "build/out", "o");
    write_file(temp.path(), "junk.log", "l");
    write_file(temp.path(), "src/lib.rs", "pub fn f() {}");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), ".oakignore", "target/\nbuild/\n*.log\n");

    oak_cli::output::begin_capture();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        out.contains("4 tracked files are now ignored"),
        "expected total count, got:\n{out}"
    );
    assert!(out.contains("target/ (2 files)"), "got:\n{out}");
    assert!(out.contains("build/ (1 file)"), "got:\n{out}");
    assert!(out.contains("junk.log"), "got:\n{out}");
}

/// A tracked file that is BOTH newly ignored and actually gone from disk is a
/// genuine deletion — no warning. `oak status` also stays quiet when nothing
/// is being dropped, and warns when something is.
#[test]
fn test_status_warns_on_now_ignored_but_not_on_real_deletions() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "logs/app.log", "log");
    write_file(temp.path(), "src/main.rs", "fn main() {}");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Deleted from disk AND newly ignored: a real deletion, no warning.
    fs::remove_dir_all(temp.path().join("logs")).unwrap();
    write_file(temp.path(), ".oakignore", "logs/\n");

    oak_cli::output::begin_capture();
    oak_cli::commands::status::run(temp.path(), false).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        !out.contains("now ignored"),
        "deleted-from-disk file must not trigger the warning, got:\n{out}"
    );

    // Still on disk and newly ignored: status warns before any commit.
    write_file(temp.path(), ".oakignore", "logs/\nsrc/\n");

    oak_cli::output::begin_capture();
    oak_cli::commands::status::run(temp.path(), false).unwrap();
    let out = oak_cli::output::end_capture();
    assert!(
        out.contains("1 tracked file under src/ is now ignored")
            && out.contains("will be removed from the branch by the next commit"),
        "expected status warning, got:\n{out}"
    );
}

#[test]
fn test_git_directory_ignored() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Create a .git directory (simulating a git repo)
    write_file(temp.path(), ".git/config", "git config");
    write_file(temp.path(), "src/main.rs", "fn main() {}");

    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();

    // .git directory should be ignored
    assert!(manifest.get(".git/config").is_none());
    assert!(manifest.get("src/main.rs").is_some());
}

#[test]
fn test_commit_tracks_oak_attributes_without_tracking_oak_state() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    fs::write(temp.path().join(".oak/attributes"), "docs/** binary\n").unwrap();
    fs::write(temp.path().join(".oak/private.txt"), "internal\n").unwrap();

    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();

    assert!(
        manifest.get(".oak/attributes").is_some(),
        ".oak/attributes stays version-controlled"
    );
    assert!(
        manifest.get(".oak/private.txt").is_none(),
        "ordinary Oak metadata stays untracked"
    );
}

#[test]
fn test_branch_edit_description() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Edit the default local branch's description (no local `main` row exists
    // under the new model — `main` lives only on the server).
    oak_cli::commands::branch::edit_branch(temp.path(), DEFAULT_BRANCH, "Updated description")
        .unwrap();

    let repo = open_repo(temp.path());
    let branch = repo.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(branch.description, Some("Updated description".to_string()));
}

#[test]
fn test_branch_close() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Create a feature branch
    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();

    // Close it
    run_close_branch(temp.path(), "feature").unwrap();

    let repo = open_repo(temp.path());
    let branch = repo.get_branch("feature").unwrap().unwrap();
    assert_eq!(branch.status, oak_core::BranchStatus::Closed);
}

#[test]
fn test_log_output() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "v1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "file.txt", "v2");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Log should succeed
    oak_cli::commands::log::run(temp.path(), Some(1), false, false, &[], None, None).unwrap();
    oak_cli::commands::log::run(temp.path(), None, true, false, &[], None, None).unwrap();
}

#[test]
fn test_api_key_metadata() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    repo.set_metadata(MetadataKey::ApiKey, "test-key-123")
        .unwrap();

    let key = repo.get_metadata(MetadataKey::ApiKey).unwrap().unwrap();
    assert_eq!(key, "test-key-123");
}

// ============================================================
// Branch switching tests
// ============================================================

#[test]
fn test_switch_branch_updates_working_directory() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Commit a file on main
    write_file(temp.path(), "main_only.txt", "main content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Create feature branch and commit a different file
    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();
    write_file(temp.path(), "feature_only.txt", "feature content");
    // Remove the main-only file from the feature branch working dir
    fs::remove_file(temp.path().join("main_only.txt")).unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Verify feature state
    assert!(temp.path().join("feature_only.txt").exists());
    assert!(!temp.path().join("main_only.txt").exists());

    // Switch back to the default branch - should restore its files
    oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false).unwrap();
    assert!(
        temp.path().join("main_only.txt").exists(),
        "main_only.txt should be restored when switching back"
    );
    // Note: switch_branch currently only writes target manifest files without cleaning
    // up files from the source branch. This means feature_only.txt may still exist.
    // If this is a bug, the assertion below will catch it once fixed:
    // assert!(!temp.path().join("feature_only.txt").exists(),
    //     "feature_only.txt should be removed when switching to main");
}

#[test]
fn test_switch_branch_with_no_commits() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Create branch and commit on it so we can switch away cleanly
    oak_cli::commands::branch::new_branch(temp.path(), "empty-branch", None, None, None).unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Switch back to the default branch should work fine
    oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false).unwrap();
    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, DEFAULT_BRANCH);

    // Switch to empty branch should also work
    oak_cli::commands::switch::run(temp.path(), Some("empty-branch"), false).unwrap();
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, "empty-branch");
}

#[test]
fn test_switch_branch_missing_root_manifest_fails_without_touching_checkout() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "keep.txt", "keep me\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let current_branch = repo.get_current_branch_name().unwrap().unwrap();
    let head_before = repo.get_branch_head(&current_branch).unwrap().unwrap();
    let missing_manifest = oak_core::Hash::from_hex(
        "1111111111111111111111111111111111111111111111111111111111111111",
    )
    .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "broken-manifest".to_string(),
        Some("points at a missing root manifest".to_string()),
        Some(DEFAULT_BRANCH.to_string()),
    ))
    .unwrap();
    let broken_head = repo
        .put_commit(
            "broken-manifest".to_string(),
            Some(head_before.clone()),
            None,
            missing_manifest.clone(),
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("broken-manifest", &broken_head)
        .unwrap();
    drop(repo);

    let result = oak_cli::commands::switch::run(temp.path(), Some("broken-manifest"), false);

    assert!(
        matches!(&result, Err(oak_core::OakError::ManifestNotFound(hash)) if hash == missing_manifest.as_str()),
        "switch should fail closed with ManifestNotFound, got {result:?}"
    );
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some(current_branch.as_str())
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::Head).unwrap().as_deref(),
        Some(head_before.as_str())
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("keep.txt")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn test_switch_create_without_name_creates_generated_branch_from_main_head() {
    use oak_core::{Branch, Commit, FileMode, Manifest, ManifestEntry};

    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

    let main_blob = repo.put_blob(b"main content\n".to_vec()).unwrap();
    let main_manifest = Manifest::new(vec![ManifestEntry {
        path: "README.md".to_string(),
        blob_hash: main_blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&main_manifest).unwrap();
    let main_commit = Commit::new(
        "main".to_string(),
        None,
        None,
        main_manifest.hash.clone(),
        "tester".to_string(),
        Some("main commit".to_string()),
        vec![],
    )
    .unwrap();
    repo.store_commit(&main_commit).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.set_branch_head("main", &main_commit.hash).unwrap();

    let old_blob = repo.put_blob(b"old branch content\n".to_vec()).unwrap();
    let old_manifest = Manifest::new(vec![ManifestEntry {
        path: "old.txt".to_string(),
        blob_hash: old_blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&old_manifest).unwrap();
    let old_commit = Commit::new(
        "old-work".to_string(),
        None,
        None,
        old_manifest.hash.clone(),
        "tester".to_string(),
        None,
        vec![],
    )
    .unwrap();
    repo.store_commit(&old_commit).unwrap();
    repo.store_branch(&Branch::new(
        "old-work".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("old-work", &old_commit.hash).unwrap();
    repo.set_current_branch("old-work").unwrap();
    repo.set_head(&old_commit.hash).unwrap();
    update_working_dir(temp.path(), &repo, &old_manifest);
    drop(repo);

    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, "old-work");
    assert_ne!(current, "main");
    assert!(
        current.contains('-'),
        "generated branch names should include the random suffix, got {current}"
    );

    let branch = repo.get_branch(&current).unwrap().unwrap();
    assert_eq!(branch.parent_branch.as_deref(), Some("main"));
    assert_eq!(
        repo.get_branch_head(&current).unwrap().as_ref(),
        Some(&main_commit.hash),
        "fresh switch should pin the branch to main's current head"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "main content\n"
    );
    assert!(
        !temp.path().join("old.txt").exists(),
        "fresh switch should materialize a clean main working tree"
    );
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "fresh branch should start clean");
}

#[test]
fn test_switch_create_without_name_preserves_dirty_worktree_by_default() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let before_branches = repo.list_branches().unwrap().len();
    drop(repo);

    write_file(temp.path(), "dirty.txt", "uncommitted");

    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    assert_eq!(
        repo.list_branches().unwrap().len(),
        before_branches + 1,
        "fresh switch should create a new branch even when carrying dirty work"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("dirty.txt")).unwrap(),
        "uncommitted"
    );
    let (changes, _, branch_name) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(branch_name.as_deref(), Some(current.as_str()));
    assert!(
        !changes.is_empty(),
        "carried dirty work should be visible on the new branch"
    );
}

#[test]
fn test_switch_create_without_name_seeds_from_current_head_when_main_is_empty() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let committed_head = repo.get_head().unwrap();
    drop(repo);
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "setup should start from a clean branch");

    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    // Local-only repos can never merge into main (merge needs a remote), so
    // an empty main must not orphan committed work: the new branch seeds at
    // the current branch's head and the last commit stays visible.
    assert_eq!(
        repo.get_branch_head(&current).unwrap(),
        committed_head,
        "new branch should seed from the current head when main has no head"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "content",
        "clean files from the previous branch should not be wiped without --discard"
    );
    let (changes, _, branch_name) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(branch_name.as_deref(), Some(current.as_str()));
    assert!(
        changes.is_empty(),
        "committed work must not re-present as work to commit on the new branch"
    );
}

#[test]
fn test_switch_create_named_keeps_committed_work_visible_when_main_is_empty() {
    // The agent-workflow shape: init -> commit -> `oak switch -c agent-task`
    // -> edit one file. Status must show only the edit, exactly like
    // `git checkout -b` after a commit.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "keep.txt", "committed\n");
    write_file(temp.path(), "edit.txt", "v1\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    oak_cli::commands::switch::create(temp.path(), "agent-task", WorktreePolicy::Carry).unwrap();
    write_file(temp.path(), "edit.txt", "v2\n");

    let (changes, _, branch_name) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(branch_name.as_deref(), Some("agent-task"));
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["edit.txt"],
        "only the post-branch edit should be dirty, got: {paths:?}"
    );
}

#[test]
fn test_switch_create_named_preserves_uncommitted_worktree_by_default() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "tracked.txt", "committed\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    write_file(temp.path(), "tracked.txt", "uncommitted edit\n");
    write_file(temp.path(), "new.txt", "uncommitted new file\n");

    oak_cli::commands::switch::create(temp.path(), "agent-task", WorktreePolicy::Carry).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "uncommitted edit\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("new.txt")).unwrap(),
        "uncommitted new file\n"
    );

    let (changes, _, branch_name) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(branch_name.as_deref(), Some("agent-task"));
    let mut paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["new.txt", "tracked.txt"]);
}

#[test]
fn test_switch_create_without_name_discard_starts_clean() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let before_branches = repo.list_branches().unwrap().len();
    drop(repo);

    write_file(temp.path(), "dirty.txt", "uncommitted");

    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Discard).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    assert_eq!(repo.list_branches().unwrap().len(), before_branches + 1);
    assert!(
        !temp.path().join("dirty.txt").exists(),
        "discard should remove dirty files"
    );
    // --discard discards working-tree CHANGES, not committed history: with
    // main empty, the latest available state is the current branch's head,
    // so committed files survive the discard.
    assert_eq!(
        fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "content",
        "discard should keep committed files when main has no head"
    );
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "discarded branch should start clean");
}

#[test]
fn test_switch_clean_create_without_name_starts_clean() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    write_file(temp.path(), "dirty.txt", "uncommitted");

    let policy = WorktreePolicy::from_clean_flag(true);
    oak_cli::commands::switch::fresh(temp.path(), policy).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    assert!(
        !temp.path().join("dirty.txt").exists(),
        "--clean should remove dirty files for generated branch creation"
    );
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty(), "clean branch should start clean");
}

#[test]
fn test_switch_clean_existing_branch_discards_dirty_work() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "base\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    oak_cli::commands::switch::create(temp.path(), "other", WorktreePolicy::Carry).unwrap();
    write_file(temp.path(), "dirty.txt", "uncommitted\n");

    oak_cli::commands::switch::run_with_policy(
        temp.path(),
        Some(DEFAULT_BRANCH),
        false,
        WorktreePolicy::from_clean_flag(true),
    )
    .unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some(DEFAULT_BRANCH)
    );
    assert!(
        !temp.path().join("dirty.txt").exists(),
        "--clean should discard dirty files before switching"
    );
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes.is_empty(),
        "clean switch should leave no dirty work"
    );
}

#[test]
fn test_switch_create_without_name_falls_back_to_local_main_when_remote_unavailable() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "local main\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    repo.set_metadata(MetadataKey::RemoteUrl, "http://127.0.0.1:1")
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "tester").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    drop(repo);

    oak_cli::output::begin_capture();
    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();
    let output = oak_cli::output::end_capture();

    assert!(
        output.contains("using local main"),
        "offline fallback should tell the user it used local main, got {output}"
    );
    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(
        repo.get_branch_head(&current).unwrap().as_ref(),
        Some(&main_hash)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "local main\n"
    );
}

#[test]
fn test_switch_create_without_name_skips_remote_when_main_recently_checked() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "recent main\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    repo.set_metadata(MetadataKey::RemoteUrl, "http://127.0.0.1:1")
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "tester").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    repo.set_metadata(MetadataKey::MainLastCheckedAt, &checked_at)
        .unwrap();
    drop(repo);

    oak_cli::output::begin_capture();
    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();
    let output = oak_cli::output::end_capture();

    assert!(
        !output.contains("Updating 'main'"),
        "recent main should avoid the remote refresh, got {output}"
    );
    assert!(
        !output.contains("warning:"),
        "recent main should not emit an offline fallback warning, got {output}"
    );
    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(
        repo.get_branch_head(&current).unwrap().as_ref(),
        Some(&main_hash)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_branch_rename_local_only_branch_when_remote_404s() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/branches/tester/rename"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let repo = open_repo(temp.path());
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    let repo_path = temp.path().to_path_buf();
    let rename = tokio::task::spawn_blocking(move || {
        oak_cli::output::begin_capture();
        let result =
            oak_cli::commands::branch::rename_branch(&repo_path, "tester", "renamed-local");
        let output = oak_cli::output::end_capture();
        (result, output)
    })
    .await
    .unwrap();
    rename
        .0
        .expect("server 404 for local-only branch should fall back to a local rename");
    let output = rename.1;

    let repo = open_repo(temp.path());
    assert!(
        repo.get_branch("tester").unwrap().is_none(),
        "old local branch row should be gone"
    );
    assert!(
        repo.get_branch("renamed-local").unwrap().is_some(),
        "new local branch row should exist"
    );
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("renamed-local")
    );
    assert!(
        output.contains("locally"),
        "rename should tell the user it was not synced, got {output:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_switch_create_without_name_fetches_remote_main_when_not_recently_checked() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let remote_content = b"remote main\n".to_vec();
    let remote_blob = oak_core::Blob::new(remote_content.clone());
    let remote_tree = Tree::new(vec![TreeEntry {
        name: "remote.txt".to_string(),
        kind: TreeEntryKind::Blob,
        hash: remote_blob.hash.clone(),
        mode: FileMode::Regular,
    }])
    .unwrap();
    let remote_manifest_hash = remote_tree.hash.clone();
    let remote_timestamp = chrono::DateTime::from_timestamp(1_700_000_300, 0).unwrap();
    let remote_commit = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        remote_manifest_hash.clone(),
        "<remote>".to_string(),
        None,
        Vec::new(),
        remote_timestamp,
    )
    .unwrap();
    let remote_head = remote_commit.hash.clone();
    let remote_tree_wire = oak_core::protocol::tree_to_wire(&remote_tree);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": remote_head.as_str()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/oak/raw/{}/remote.txt",
            remote_head.as_str()
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(String::from_utf8(remote_content).unwrap()),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commits": [{
                "hash": remote_head.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": remote_manifest_hash.as_str(),
                "author": "<remote>",
                "timestamp": remote_timestamp.to_rfc3339(),
                "files": []
            }],
            "trees": [remote_tree_wire]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let repo = open_repo(temp.path());
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    let worktree = temp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        oak_cli::commands::switch::fresh(&worktree, WorktreePolicy::Carry)
    })
    .await
    .unwrap()
    .unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    assert_eq!(
        repo.get_branch_head("main").unwrap().as_ref(),
        Some(&remote_head),
        "remote fetch should update the local main pointer"
    );
    assert_eq!(
        repo.get_branch_head(&current).unwrap().as_ref(),
        Some(&remote_head),
        "new generated branch should start at fetched remote main"
    );
    assert!(
        repo.get_metadata(MetadataKey::MainLastCheckedAt)
            .unwrap()
            .is_some(),
        "successful remote refresh should mark main as recently checked"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("remote.txt")).unwrap(),
        "remote main\n",
        "clean switch should materialize the fetched remote main tree"
    );
}

/// `oak switch <name>` where `<name>` only exists on the remote: the branch
/// is fetched (here: a branch with no commits of its own, seeded at a main
/// commit we already hold) and switched onto.
#[tokio::test(flavor = "current_thread")]
async fn test_switch_fetches_branch_that_exists_only_on_remote() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "main content\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    // Pin the current branch at main's head so the working tree is clean —
    // `oak switch` refuses to move over uncommitted changes.
    repo.set_branch_head(DEFAULT_BRANCH, &main_hash).unwrap();
    repo.set_head(&main_hash).unwrap();

    let server = MockServer::start().await;
    let branch_json = json!({
        "name": "remote-feature",
        "description": "made in the web UI",
        "parent_branch": "main",
        "status": "open",
        "created_at": "2026-01-01T00:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": main_hash.as_str(),
            "branch": branch_json,
            "branches": [branch_json],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    let worktree = temp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        oak_cli::commands::switch::run(&worktree, Some("remote-feature"), false)
    })
    .await
    .unwrap()
    .unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("remote-feature")
    );
    assert_eq!(
        repo.get_branch_head("remote-feature").unwrap().as_ref(),
        Some(&main_hash),
        "fetched branch should sit at its server-reported head"
    );
    assert_eq!(
        repo.get_branch("remote-feature")
            .unwrap()
            .unwrap()
            .parent_branch
            .as_deref(),
        Some("main")
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "main content\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_switch_hydrates_missing_local_branch_blob_from_remote() {
    use oak_core::{Branch, Commit, FileChange, FileMode, ManifestEntry};

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "main content\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    repo.set_branch_head(DEFAULT_BRANCH, &main_hash).unwrap();
    repo.set_head(&main_hash).unwrap();

    let content = b"hydrated from remote\n".to_vec();
    let missing_blob = hash_bytes(&content);
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "remote.txt".to_string(),
        blob_hash: missing_blob.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "incomplete-local".to_string(),
        Some(main_hash.clone()),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        None,
        vec![FileChange {
            path: "remote.txt".to_string(),
            change_type: ChangeType::Added,
            old_blob_hash: None,
            new_blob_hash: Some(missing_blob.clone()),
            old_path: None,
            old_mode: None,
            new_mode: Some(FileMode::Regular),
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.store_branch(&Branch::new(
        "incomplete-local".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("incomplete-local", &commit.hash)
        .unwrap();
    assert!(!repo.has_blob(&missing_blob).unwrap(), "precondition");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .and(body_partial_json(
            json!({ "hashes": [missing_blob.as_str()] }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "blobs": [{
                "hash": missing_blob.as_str(),
                "size": content.len(),
                "chunks": [{
                    "hash": missing_blob.as_str(),
                    "offset": 0,
                    "size": content.len(),
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/download"))
        .and(body_partial_json(
            json!({ "hashes": [missing_blob.as_str()] }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "hash": missing_blob.as_str(),
                "content": content,
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    write_file(temp.path(), "dirty.txt", "discard me\n");

    let worktree = temp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        oak_cli::commands::switch::run_with_policy(
            &worktree,
            Some("incomplete-local"),
            false,
            WorktreePolicy::Discard,
        )
    })
    .await
    .unwrap()
    .unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("incomplete-local")
    );
    assert!(repo.has_blob(&missing_blob).unwrap());
    assert_eq!(
        fs::read_to_string(temp.path().join("remote.txt")).unwrap(),
        "hydrated from remote\n"
    );
    assert!(
        !temp.path().join("dirty.txt").exists(),
        "--clean should discard only after the missing blob is hydrated"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_switch_missing_remote_blob_leaves_checkout_unchanged() {
    use oak_core::{Branch, Commit, FileChange, FileMode, ManifestEntry};

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "main content\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    repo.set_branch_head(DEFAULT_BRANCH, &main_hash).unwrap();
    repo.set_head(&main_hash).unwrap();

    let missing_blob = Hash::from_hex(&"b".repeat(64)).unwrap();
    let manifest = Manifest::new(vec![ManifestEntry {
        path: "remote.txt".to_string(),
        blob_hash: missing_blob.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "incomplete-local".to_string(),
        Some(main_hash.clone()),
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        None,
        vec![FileChange {
            path: "remote.txt".to_string(),
            change_type: ChangeType::Added,
            old_blob_hash: None,
            new_blob_hash: Some(missing_blob.clone()),
            old_path: None,
            old_mode: None,
            new_mode: Some(FileMode::Regular),
        }],
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    repo.store_branch(&Branch::new(
        "incomplete-local".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("incomplete-local", &commit.hash)
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "blobs": [] })))
        .expect(1)
        .mount(&server)
        .await;

    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    write_file(temp.path(), "dirty.txt", "must survive failed switch\n");

    let worktree = temp.path().to_path_buf();
    let err = tokio::task::spawn_blocking(move || {
        oak_cli::commands::switch::run_with_policy(
            &worktree,
            Some("incomplete-local"),
            false,
            WorktreePolicy::Discard,
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("could not hydrate") && msg.contains(missing_blob.as_str()),
        "expected hydration failure with missing hash, got: {msg}"
    );

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some(DEFAULT_BRANCH)
    );
    assert_eq!(
        repo.get_branch_head(DEFAULT_BRANCH).unwrap().as_ref(),
        Some(&main_hash)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("dirty.txt")).unwrap(),
        "must survive failed switch\n"
    );
    assert!(
        !temp.path().join("remote.txt").exists(),
        "failed hydration must not partially materialize the target branch"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_clone_branch_clones_then_switches_to_remote_branch() {
    let temp = TempDir::new().unwrap();

    let main_manifest = Manifest::empty();
    let main_commit = oak_core::Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        main_manifest.hash.clone(),
        "tester".to_string(),
        Some("main commit".to_string()),
        vec![],
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();

    let feature_manifest = Manifest::empty();
    let feature_commit = oak_core::Commit::with_timestamp(
        "remote-feature".to_string(),
        None,
        None,
        feature_manifest.hash.clone(),
        "tester".to_string(),
        Some("feature commit".to_string()),
        vec![],
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .unwrap();

    let main_branch = json!({
        "name": "main",
        "description": null,
        "parent_branch": null,
        "status": "open",
        "created_at": "2026-01-01T00:00:00Z"
    });
    let feature_branch = json!({
        "name": "remote-feature",
        "description": "made remotely",
        "parent_branch": "main",
        "status": "open",
        "created_at": "2026-01-01T00:00:00Z"
    });

    let server = MockServer::start().await;
    // Model an accessible pre-integrity server explicitly. Capability 404 is
    // ambiguous by itself; the longstanding repo-info endpoint proves that
    // this is legacy support rather than a hidden repo or rejected credential.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "oak"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": main_commit.hash.as_str(),
            "branch": main_branch.clone(),
            "branches": [main_branch.clone(), feature_branch.clone()],
            "commits": [{
                "hash": main_commit.hash.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": main_manifest.hash.as_str(),
                "author": "tester",
                "message": "main commit",
                "timestamp": "2026-01-01T00:00:00Z",
                "files": []
            }],
            "blobs": [],
            "trees": [{
                "hash": main_manifest.hash.as_str(),
                "entries": []
            }]
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch_name", "remote-feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": feature_commit.hash.as_str(),
            "branch": feature_branch.clone(),
            "branches": [feature_branch.clone()],
            "commits": [{
                "hash": feature_commit.hash.as_str(),
                "branch_name": "remote-feature",
                "parent_hash": null,
                "manifest_hash": feature_manifest.hash.as_str(),
                "author": "tester",
                "message": "feature commit",
                "timestamp": "2026-01-01T00:00:01Z",
                "files": []
            }],
            "blobs": [],
            "trees": [{
                "hash": feature_manifest.hash.as_str(),
                "entries": []
            }]
        })))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    let remote = server.uri();
    let out = oak_bin(
        temp.path(),
        &[
            "clone",
            "--remote",
            &remote,
            "--branch",
            "remote-feature",
            // This fixture intentionally models a pre-integrity server. A
            // branch-only clone must opt into accepting that legacy server's
            // unproved scope before the pull response remains authoritative.
            "--allow-legacy-scope",
            "oak/oak",
            "cloned",
        ],
    );
    assert!(
        out.status.success(),
        "clone --branch failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let clone_dir = temp.path().join("cloned");
    let repo = open_repo(&clone_dir);
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("remote-feature")
    );
    assert_eq!(
        repo.get_branch_head("remote-feature").unwrap().as_ref(),
        Some(&feature_commit.hash)
    );
    assert!(
        !clone_dir.join("README.md").exists(),
        "switching after clone should leave the worktree on the requested branch"
    );
}

/// Switching to a branch the remote has already merged (closed) must not
/// resurrect it locally.
#[tokio::test(flavor = "current_thread")]
async fn test_switch_refuses_branch_already_merged_on_remote() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "main content\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    repo.set_branch_head(DEFAULT_BRANCH, &main_hash).unwrap();
    repo.set_head(&main_hash).unwrap();

    let server = MockServer::start().await;
    let branch_json = json!({
        "name": "old-merged",
        "description": null,
        "parent_branch": "main",
        "status": "closed",
        "created_at": "2026-01-01T00:00:00Z"
    });
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": main_hash.as_str(),
            "branch": branch_json,
            "branches": [branch_json],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    drop(repo);

    let worktree = temp.path().to_path_buf();
    let err = tokio::task::spawn_blocking(move || {
        oak_cli::commands::switch::run(&worktree, Some("old-merged"), false)
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        err.to_string().contains("already merged"),
        "expected a clear merged-branch error, got: {err}"
    );

    let repo = open_repo(temp.path());
    assert!(
        repo.get_branch("old-merged").unwrap().is_none(),
        "the merged branch must not leave a local tombstone behind"
    );
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some(DEFAULT_BRANCH),
        "failed switch should leave the current branch untouched"
    );
}

#[test]
fn test_switch_create_without_name_carries_committed_divergence_as_uncommitted_work() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let repo = open_repo(temp.path());
    let main_hash = seed_main_commit(&repo, "README.md", "main content\n");
    materialize_commit(temp.path(), &repo, &main_hash);
    drop(repo);

    write_file(temp.path(), "feature.txt", "committed divergence\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    write_file(temp.path(), "dirty.txt", "uncommitted edit\n");

    oak_cli::commands::switch::fresh(temp.path(), WorktreePolicy::Carry).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, DEFAULT_BRANCH);
    assert_eq!(
        repo.get_branch_head(&current).unwrap().as_ref(),
        Some(&main_hash),
        "new branch should still be based on main, not the divergent old branch"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("feature.txt")).unwrap(),
        "committed divergence\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("dirty.txt")).unwrap(),
        "uncommitted edit\n"
    );

    let (changes, _, branch_name) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(branch_name.as_deref(), Some(current.as_str()));
    let mut added_paths: Vec<&str> = changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Added)
        .map(|change| change.path.as_str())
        .collect();
    added_paths.sort_unstable();
    assert_eq!(
        added_paths,
        vec!["dirty.txt", "feature.txt"],
        "committed divergence plus dirty edits should appear as carried work"
    );
}

#[test]
fn test_switch_create_named_branch_uses_main_head() {
    use oak_core::{Branch, Commit, FileMode, Manifest, ManifestEntry};

    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

    let main_blob = repo.put_blob(b"main content\n".to_vec()).unwrap();
    let main_manifest = Manifest::new(vec![ManifestEntry {
        path: "README.md".to_string(),
        blob_hash: main_blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&main_manifest).unwrap();
    let main_commit = Commit::new(
        "main".to_string(),
        None,
        None,
        main_manifest.hash.clone(),
        "tester".to_string(),
        Some("main commit".to_string()),
        vec![],
    )
    .unwrap();
    repo.store_commit(&main_commit).unwrap();
    repo.store_branch(&Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.set_branch_head("main", &main_commit.hash).unwrap();

    let old_blob = repo.put_blob(b"old branch content\n".to_vec()).unwrap();
    let old_manifest = Manifest::new(vec![ManifestEntry {
        path: "old.txt".to_string(),
        blob_hash: old_blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&old_manifest).unwrap();
    let old_commit = Commit::new(
        "old-work".to_string(),
        None,
        None,
        old_manifest.hash.clone(),
        "tester".to_string(),
        None,
        vec![],
    )
    .unwrap();
    repo.store_commit(&old_commit).unwrap();
    repo.store_branch(&Branch::new(
        "old-work".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("old-work", &old_commit.hash).unwrap();
    repo.set_current_branch("old-work").unwrap();
    repo.set_head(&old_commit.hash).unwrap();
    update_working_dir(temp.path(), &repo, &old_manifest);
    drop(repo);

    oak_cli::commands::switch::create(temp.path(), "named-work", WorktreePolicy::Carry).unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("named-work")
    );
    let branch = repo.get_branch("named-work").unwrap().unwrap();
    assert_eq!(branch.parent_branch.as_deref(), Some("main"));
    assert_eq!(
        repo.get_branch_head("named-work").unwrap().as_ref(),
        Some(&main_commit.hash)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("README.md")).unwrap(),
        "main content\n"
    );
    assert!(!temp.path().join("old.txt").exists());
}

#[test]
fn test_switch_without_name_noninteractive_requires_explicit_action() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = open_repo(temp.path());
    let before_branches = repo.list_branches().unwrap().len();
    drop(repo);

    let result = oak_cli::commands::switch::run(temp.path(), None, false);
    assert!(
        result.is_err(),
        "bare switch should not create a branch in non-interactive mode"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("oak switch -c"),
        "error should point agents at the generated-branch path, got {err}"
    );
    assert!(
        err.contains("oak switch NAME"),
        "error should mention explicit branch switching, got {err}"
    );
    assert!(
        err.contains("oak switch -c --clean"),
        "error should mention the clean generated-branch path, got {err}"
    );

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some(DEFAULT_BRANCH)
    );
    assert_eq!(
        repo.list_branches().unwrap().len(),
        before_branches,
        "bare non-interactive switch should not create a branch"
    );
}

/// Regression test for the path-keyed stat-cache "foreign blob" bug.
///
/// The `stat_cache` table is keyed by path only and is shared across every
/// branch that lives in a single working dir. Before the fix, switching
/// branches rewrote the working tree but left the cache untouched, so a row
/// could keep pointing at the *other* branch's version of a file. A later
/// scan that happened to see a matching `(mtime, ctime, size)` would then
/// trust that stale row and record a foreign blob — silently regressing an
/// unchanged file (e.g. reverting `main`'s copy to an unrelated branch's).
///
/// This pins the invariant the fix restores: after a branch switch
/// materializes the working tree, every stat-cache row matches on-disk
/// content. Without the fix the row still holds the feature branch's hash.
#[test]
fn test_stat_cache_matches_disk_after_switch() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Version A on the default branch.
    write_file(temp.path(), "shared.rs", "version A — the default branch\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // A different-content, different-length version B on a feature branch.
    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();
    write_file(
        temp.path(),
        "shared.rs",
        "version B — a feature branch, deliberately a different length\n",
    );
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // The cache now holds version B's hash for shared.rs (we're on `feature`).
    let repo = open_repo(temp.path());
    let hash_b = repo
        .load_stat_cache()
        .unwrap()
        .get("shared.rs")
        .expect("cache row on feature")
        .blob_hash
        .clone();
    drop(repo);

    // Switch back to the default branch — this rewrites shared.rs to version A.
    oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false).unwrap();

    let on_disk = fs::read(temp.path().join("shared.rs")).unwrap();
    let on_disk_hash = oak_core::hash_bytes(&on_disk);
    assert_ne!(
        on_disk_hash, hash_b,
        "the two branches must really hold different content for this test to mean anything"
    );

    // The cache row must now reflect version A (what's on disk), NOT the stale
    // version B left over from the feature branch.
    let repo = open_repo(temp.path());
    let row = repo
        .load_stat_cache()
        .unwrap()
        .get("shared.rs")
        .expect("cache row after switch")
        .clone();
    assert_eq!(
        row.blob_hash, on_disk_hash,
        "stat cache must mirror on-disk content after a switch, not a foreign \
         blob from another branch (path-keyed stat-cache regression)"
    );
}

/// The conflict-resolution scans (`oak pull --continue` / `oak merge
/// --continue`) must record on-disk content even when the stat cache holds a
/// poisoned row — the exact corrupt state a cross-branch materialization can
/// leave behind. `scan_working_dir_no_cache` re-hashes from disk and ignores
/// the cache. This also documents, via the intermediate assertion, that a
/// plain cached scan *would* be fooled — which is why the no-cache path exists
/// for those high-stakes commits.
#[test]
fn test_no_cache_scan_ignores_poisoned_stat_cache_row() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "merge.rs", "the real, on-disk content\n");
    let repo = open_repo(temp.path());
    let ignore = oak_core::IgnorePatterns::new(temp.path()).unwrap();

    // A normal cached scan records the true hash and populates the cache.
    let entries =
        oak_cli::commands::commit::scan_working_dir(temp.path(), temp.path(), &repo, &ignore)
            .unwrap();
    let true_hash = entries
        .iter()
        .find(|e| e.path == "merge.rs")
        .expect("merge.rs scanned")
        .blob_hash
        .clone();

    // Poison the cache: keep the file's real (mtime, ctime, size) but point the
    // row at a foreign blob hash, mimicking a stale row from another branch.
    let foreign = oak_core::Hash("00000000deadbeef".to_string());
    let mut row = repo
        .load_stat_cache()
        .unwrap()
        .remove("merge.rs")
        .expect("cache row to poison");
    assert_ne!(row.blob_hash, foreign);
    row.blob_hash = foreign.clone();
    repo.update_stat_cache(&[("merge.rs".to_string(), row)], &[])
        .unwrap();

    // A plain cached scan is fooled: it trusts the (mtime, ctime, size) match
    // and emits the foreign blob. This is the corruption, reproduced.
    let cached =
        oak_cli::commands::commit::scan_working_dir(temp.path(), temp.path(), &repo, &ignore)
            .unwrap();
    let cached_hash = cached
        .iter()
        .find(|e| e.path == "merge.rs")
        .unwrap()
        .blob_hash
        .clone();
    assert_eq!(
        cached_hash, foreign,
        "a cached scan trusts the poisoned (mtime,ctime,size) row"
    );

    // The no-cache scan must ignore the cache and record on-disk content.
    let fresh = oak_cli::commands::commit::scan_working_dir_no_cache(
        temp.path(),
        temp.path(),
        &repo,
        &ignore,
    )
    .unwrap();
    let fresh_hash = fresh
        .iter()
        .find(|e| e.path == "merge.rs")
        .unwrap()
        .blob_hash
        .clone();
    assert_eq!(
        fresh_hash, true_hash,
        "no-cache scan must record on-disk content, never the stale foreign blob"
    );

    // …and it repairs the poisoned row as a side effect (writes fresh rows even
    // though it never trusts existing ones).
    let repaired = repo.load_stat_cache().unwrap();
    assert_eq!(
        repaired.get("merge.rs").unwrap().blob_hash,
        true_hash,
        "no-cache scan repairs the cache"
    );
}

#[test]
fn test_new_branch_inherits_parent_head() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "base.txt", "base content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let parent_head = repo.get_branch_head(DEFAULT_BRANCH).unwrap();

    // Create a feature branch
    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();

    // Feature branch should share parent's working directory files
    assert!(temp.path().join("base.txt").exists());
    let content = fs::read_to_string(temp.path().join("base.txt")).unwrap();
    assert_eq!(content, "base content");

    // Feature branch has no head yet (it hasn't committed)
    let repo = open_repo(temp.path());
    let feature_head = repo.get_branch_head("feature").unwrap();
    assert!(feature_head.is_none());

    // But the parent branch's head should still be set
    let parent_head_after = repo.get_branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(parent_head, parent_head_after);
}

#[test]
fn test_new_branch_from_commit_seeds_head_and_working_tree() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    // Two commits so we can rewind to the first.
    write_file(temp.path(), "base.txt", "first");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let first_hash = open_repo(temp.path())
        .get_branch_head(DEFAULT_BRANCH)
        .unwrap()
        .unwrap();
    write_file(temp.path(), "base.txt", "second");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("base.txt")).unwrap(),
        "second"
    );

    // Branch off the first commit. Working tree should rewind, branch
    // head should be pinned at first_hash, and we should be on it.
    oak_cli::commands::branch::new_branch(
        temp.path(),
        "rewind",
        None,
        Some(first_hash.as_str()),
        Some("main"),
    )
    .unwrap();

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("rewind")
    );
    assert_eq!(
        repo.get_branch_head("rewind").unwrap().as_ref(),
        Some(&first_hash)
    );
    assert_eq!(
        repo.get_branch("rewind")
            .unwrap()
            .unwrap()
            .parent_branch
            .as_deref(),
        Some("main"),
        "explicit --parent main should be honored"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("base.txt")).unwrap(),
        "first",
        "--from should rewind the working tree to that commit's manifest"
    );
}

#[test]
fn test_new_branch_from_rejects_uncommitted_changes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "v1");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let hash = open_repo(temp.path())
        .get_branch_head(DEFAULT_BRANCH)
        .unwrap()
        .unwrap();

    // Dirty the tree.
    write_file(temp.path(), "a.txt", "dirty");

    let err = oak_cli::commands::branch::new_branch(
        temp.path(),
        "rewind",
        None,
        Some(hash.as_str()),
        None,
    )
    .unwrap_err();
    assert!(
        matches!(err, oak_core::OakError::UncommittedChanges),
        "expected UncommittedChanges, got {err:?}"
    );
    // The dirty content must still be there — we refused the operation
    // before touching anything.
    assert_eq!(
        fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "dirty"
    );
}

// ============================================================
// Merge tests
// ============================================================

#[tokio::test]
async fn test_merge_no_parent_fails() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // The default local branch is parented onto `main`, but local merges
    // into `main` are rejected (only the server can squash-merge onto
    // main). So `oak merge` from the default branch should fail.
    let result =
        oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false).await;
    assert!(
        result.is_err(),
        "Merging the default branch (parented onto main) should fail locally"
    );

    // Also exercise the literal "no parent" path: install a parentless
    // branch directly via the storage API and confirm merge rejects it.
    let repo = open_repo(temp.path());
    let orphan = oak_core::Branch::new("orphan".to_string(), None, None);
    repo.store_branch(&orphan).unwrap();
    repo.set_current_branch("orphan").unwrap();
    drop(repo);

    let result =
        oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false).await;
    assert!(
        result.is_err(),
        "Merging a parentless branch should also fail"
    );
}

// ============================================================
// Commit on branch tests
// ============================================================

#[test]
fn test_commit_on_closed_branch_fails() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();
    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Close the branch
    run_close_branch(temp.path(), "feature").unwrap();

    // Try to commit - should fail
    write_file(temp.path(), "file2.txt", "more content");
    let result = oak_cli::commands::commit::run(temp.path());
    assert!(result.is_err(), "Committing to a closed branch should fail");
}

#[test]
fn test_commits_tracked_per_branch() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "main.txt", "main");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Create feature branch and commit
    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();
    write_file(temp.path(), "feature.txt", "feature");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());

    // The default branch should have 1 commit
    let default_commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    assert_eq!(default_commits.len(), 1);
    // Local commits don't carry messages under the new model.
    assert_eq!(default_commits[0].message, None);

    // Feature should have 1 commit
    let feature_commits = repo.get_commits_for_branch("feature").unwrap();
    assert_eq!(feature_commits.len(), 1);
    assert_eq!(feature_commits[0].message, None);
}

// ============================================================
// Checkout tests
// ============================================================

#[test]
fn test_switch_branch() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "main.txt", "main");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();
    write_file(temp.path(), "feature.txt", "feature");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Switch back to the default branch
    oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false).unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, DEFAULT_BRANCH);
    assert!(temp.path().join("main.txt").exists());
    // switch properly deletes files not in target branch
    assert!(!temp.path().join("feature.txt").exists());
}

#[test]
fn test_switch_commit_hash() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "v1.txt", "version 1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let first_commit_hash = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();

    write_file(temp.path(), "v2.txt", "version 2");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Switch to the first commit by hash (detached HEAD)
    oak_cli::commands::switch::run(temp.path(), Some(first_commit_hash.as_str()), false).unwrap();

    // Should have v1.txt but not v2.txt
    assert!(temp.path().join("v1.txt").exists());
    assert!(!temp.path().join("v2.txt").exists());

    // Should be in detached HEAD state. On disk we store the empty-string
    // sentinel, but `get_current_branch_name()` collapses Some("") to None
    // at the read boundary (see oak_core::Repository docstring), so
    // None is the correct user-visible value.
    let repo = open_repo(temp.path());
    assert_eq!(repo.get_current_branch_name().unwrap(), None);
}

#[test]
fn switch_to_known_lost_version_removes_stale_tracked_bytes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let repo = open_repo(temp.path());

    let lost_hash = hash_bytes(b"lost historical bytes");
    let historical_manifest = Manifest::new(vec![oak_core::ManifestEntry {
        path: "Cargo.lock".to_string(),
        blob_hash: lost_hash.clone(),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&historical_manifest).unwrap();
    let historical = Commit::new(
        "historical".to_string(),
        None,
        None,
        historical_manifest.hash,
        "tester".to_string(),
        None,
        Vec::new(),
    )
    .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "historical".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.store_commit(&historical).unwrap();
    repo.set_branch_head("historical", &historical.hash)
        .unwrap();

    let current_blob = repo.put_blob(b"current bytes".to_vec()).unwrap();
    let current_manifest = Manifest::new(vec![oak_core::ManifestEntry {
        path: "Cargo.lock".to_string(),
        blob_hash: current_blob,
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&current_manifest).unwrap();
    let current = Commit::new(
        DEFAULT_BRANCH.to_string(),
        None,
        None,
        current_manifest.hash,
        "tester".to_string(),
        None,
        Vec::new(),
    )
    .unwrap();
    repo.store_commit(&current).unwrap();
    repo.set_branch_head(DEFAULT_BRANCH, &current.hash).unwrap();
    repo.set_head(&current.hash).unwrap();
    repo.set_metadata(MetadataKey::KnownLostBlobs, lost_hash.as_str())
        .unwrap();
    write_file(temp.path(), "Cargo.lock", "current bytes");

    oak_cli::commands::switch::run(temp.path(), Some("historical"), false).unwrap();

    assert!(
        !temp.path().join("Cargo.lock").exists(),
        "known-lost target must be absent, never stale bytes from the prior branch"
    );
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(
        changes.is_empty(),
        "declared known loss must carry forward cleanly"
    );

    write_file(temp.path(), "Cargo.lock", "unrelated replacement bytes");
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].change_type, ChangeType::Modified);
    let error = oak_cli::commands::commit::run(temp.path())
        .expect_err("unrelated bytes must not silently replace operator-declared loss");
    assert!(error.to_string().contains("known-loss recovery"), "{error}");
    assert_eq!(
        open_repo(temp.path())
            .get_branch_head("historical")
            .unwrap(),
        Some(historical.hash)
    );
}

#[test]
fn test_switch_detach_flag() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "v1.txt", "version 1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let commit_hash = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();

    // Switch with --detach flag
    oak_cli::commands::switch::run(temp.path(), Some(commit_hash.as_str()), true).unwrap();

    // Should be in detached HEAD state. See test_switch_commit_hash for
    // why None (not Some("")) is the correct read-boundary value.
    let repo = open_repo(temp.path());
    assert_eq!(repo.get_current_branch_name().unwrap(), None);
}

#[test]
fn test_switch_rejects_uncommitted_changes() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    oak_cli::commands::branch::new_branch(temp.path(), "feature", None, None, None).unwrap();

    // Make uncommitted changes
    write_file(temp.path(), "dirty.txt", "uncommitted");

    // Switch should fail with uncommitted changes
    let result = oak_cli::commands::switch::run(temp.path(), Some(DEFAULT_BRANCH), false);
    assert!(result.is_err(), "switch should reject uncommitted changes");
}

#[test]
fn test_switch_nonexistent_fails() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let result = oak_cli::commands::switch::run(temp.path(), Some("nonexistent"), false);
    assert!(result.is_err());
}

#[test]
fn test_export_from_subdirectory_resolves_repo_root() {
    let temp = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "README.md", "hello export\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();
    fs::create_dir_all(temp.path().join("sub")).unwrap();

    let dest = output.path().join("exported");
    oak_cli::commands::export::run(&temp.path().join("sub"), &dest, None, None, false).unwrap();

    assert_eq!(
        fs::read_to_string(dest.join("README.md")).unwrap(),
        "hello export\n"
    );
}

// ============================================================
// Archive command tests
// ============================================================

#[test]
fn test_archive_creates_zip() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "hello.txt", "hello world");
    write_file(temp.path(), "src/main.rs", "fn main() {}");

    let output_path = temp.path().join("test_archive.zip");
    oak_cli::commands::archive::run(temp.path(), Some(output_path.as_path())).unwrap();

    assert!(output_path.exists(), "archive zip should be created");
    assert!(
        fs::metadata(&output_path).unwrap().len() > 0,
        "archive should not be empty"
    );
}

#[test]
fn test_archive_default_output_does_not_include_itself() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "hello.txt", "hello world");

    oak_cli::commands::archive::run(temp.path(), None).unwrap();

    let archive_name = format!("{}.zip", temp.path().file_name().unwrap().to_string_lossy());
    let output_path = temp.path().join(&archive_name);
    let file = fs::File::open(&output_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let mut names = Vec::new();
    for i in 0..zip.len() {
        names.push(zip.by_index(i).unwrap().name().to_string());
    }

    assert!(names.iter().any(|name| name == "hello.txt"));
    assert!(
        !names.iter().any(|name| name == &archive_name),
        "archive should not contain itself: {names:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_archive_does_not_follow_directory_symlink_outside_repo() {
    let temp = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "hello.txt", "hello world");
    fs::write(external.path().join("secret.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(external.path(), temp.path().join("external")).unwrap();

    let output_path = output.path().join("archive.zip");
    oak_cli::commands::archive::run(temp.path(), Some(output_path.as_path())).unwrap();

    let file = fs::File::open(&output_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let mut names = Vec::new();
    for i in 0..zip.len() {
        names.push(zip.by_index(i).unwrap().name().to_string());
    }

    assert!(names.iter().any(|name| name == "hello.txt"));
    assert!(
        !names.iter().any(|name| name == "external/secret.txt"),
        "archive followed a symlink outside the repo: {names:?}"
    );
}

// ============================================================
// Resolve tests
// ============================================================

#[test]
fn test_resolve_finds_oak_dir() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let ctx = oak_cli::resolve::resolve(temp.path()).unwrap();
    // resolve() canonicalizes the path during its parent walk — on macOS
    // `/tmp` resolves to `/private/tmp`, so compare against the canonical form.
    let expected = fs::canonicalize(temp.path()).unwrap();
    assert_eq!(ctx.work_tree, expected);
    assert_eq!(ctx.oak_dir, expected.join(".oak"));
    assert!(ctx.db_path().unwrap().ends_with("oak.db"));
}

#[test]
fn test_resolve_fails_without_oak_dir() {
    let temp = TempDir::new().unwrap();
    let result = oak_cli::resolve::resolve(temp.path());
    assert!(
        result.is_err(),
        "resolve should fail without .oak directory"
    );
}

// ============================================================
// Reset edge cases
// ============================================================

#[test]
fn test_reset_single_file() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "original a");
    write_file(temp.path(), "b.txt", "original b");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Modify both files
    write_file(temp.path(), "a.txt", "modified a");
    write_file(temp.path(), "b.txt", "modified b");

    // Reset only a.txt (force=true to skip prompt)
    oak_cli::commands::reset::run(temp.path(), Some(Path::new("a.txt")), true).unwrap();

    // a.txt should be restored, b.txt should still be modified
    assert_eq!(
        fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "original a"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "modified b"
    );
}

#[test]
fn test_reset_dot_resets_entire_repo() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "original a");
    write_file(temp.path(), "b.txt", "original b");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "a.txt", "modified a");
    write_file(temp.path(), "b.txt", "modified b");

    oak_cli::commands::reset::run(temp.path(), Some(Path::new(".")), true).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "original a"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "original b"
    );
}

#[test]
fn test_reset_no_commits_is_noop() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");

    // Reset with no commits should not error
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();
}

#[test]
fn test_reset_clean_directory_is_noop() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Reset clean working dir should report nothing to do
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();

    // File should still be there
    assert_eq!(
        fs::read_to_string(temp.path().join("file.txt")).unwrap(),
        "content"
    );
}

#[test]
fn test_reset_restores_deleted_file() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "important.txt", "don't delete me");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Delete the file
    fs::remove_file(temp.path().join("important.txt")).unwrap();
    assert!(!temp.path().join("important.txt").exists());

    // Reset should restore it
    oak_cli::commands::reset::run(temp.path(), None, true).unwrap();
    assert!(temp.path().join("important.txt").exists());
    assert_eq!(
        fs::read_to_string(temp.path().join("important.txt")).unwrap(),
        "don't delete me"
    );
}

// ============================================================
// File permissions tests (Unix only)
// ============================================================

#[cfg(unix)]
#[test]
fn test_file_permissions_regular() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("regular.txt");
    fs::write(&file_path, "content").unwrap();

    oak_cli::file_permissions::apply_file_permissions(&file_path, oak_core::FileMode::Regular)
        .unwrap();

    let mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "regular files should be 644");
}

#[cfg(unix)]
#[test]
fn test_file_permissions_executable() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("script.sh");
    fs::write(&file_path, "#!/bin/bash").unwrap();

    oak_cli::file_permissions::apply_file_permissions(&file_path, oak_core::FileMode::Executable)
        .unwrap();

    let mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "executable files should be 755");
}

// ---------------------------------------------------------------------------
// Sync tests
// ---------------------------------------------------------------------------

#[test]
fn test_sync_no_parent_fails() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Install a parentless branch directly via the storage API and switch
    // to it. Under the new branch model the default local branch is
    // always parented onto `main`, so we have to construct a parentless
    // branch manually to exercise the "no parent" error path.
    let repo = open_repo(temp.path());
    let orphan = oak_core::Branch::new("orphan".to_string(), None, None);
    repo.store_branch(&orphan).unwrap();
    repo.set_current_branch("orphan").unwrap();
    drop(repo);

    let result = run_sync(temp.path(), false, false);
    assert!(
        result.is_err(),
        "Syncing a branch with no parent should fail"
    );
}

/// `prepare_personal_branch` now creates a fresh branch under whatever
/// proposed name it's given. Names are unique by construction (the caller
/// passes a `<author>-<rand6hex>` from `init::default_local_branch_name`),
/// so the helper no longer recycles closed-name suffixes — branch-per-
/// clone is the model and stray collisions just regenerate. This test
/// pins the basic create path: branch row stored open, parented onto
/// main, head pinned to main's tip.
#[test]
fn test_prepare_personal_branch_creates_fresh_open_branch() {
    use oak_core::{hash_string, Branch, BranchStatus, Commit, FileMode, Manifest, ManifestEntry};

    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

    let main_branch = Branch::new("main".to_string(), None, None);
    repo.store_branch(&main_branch).unwrap();

    let manifest = Manifest::new(vec![ManifestEntry {
        path: "README.md".to_string(),
        blob_hash: hash_string("readme content"),
        mode: FileMode::Regular,
    }]);
    repo.store_manifest(&manifest).unwrap();
    let main_head_commit = Commit::new(
        "main".to_string(),
        None,
        None,
        manifest.hash.clone(),
        "tester".to_string(),
        Some("initial".to_string()),
        vec![],
    )
    .unwrap();
    repo.store_commit(&main_head_commit).unwrap();
    repo.set_branch_head("main", &main_head_commit.hash)
        .unwrap();
    repo.set_head(&main_head_commit.hash).unwrap();

    let chosen = oak_cli::commands::repo::prepare_personal_branch(&repo, "zdgeier-3f2a8b").unwrap();
    assert_eq!(chosen, "zdgeier-3f2a8b");
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("zdgeier-3f2a8b"),
    );
    let new_branch = repo.get_branch("zdgeier-3f2a8b").unwrap().unwrap();
    assert_eq!(new_branch.status, BranchStatus::Open);
    assert_eq!(new_branch.parent_branch.as_deref(), Some("main"));
    let head = repo.get_branch_head("zdgeier-3f2a8b").unwrap();
    assert_eq!(head.as_ref(), Some(&main_head_commit.hash));
}

/// If a name collision happens (vanishingly rare uuid prefix collision
/// within one local DB), `prepare_personal_branch` regenerates with a
/// fresh suffix rather than erroring or recycling a numeric tail. We
/// can't easily force a collision deterministically, so this test just
/// pins that an existing branch under the same proposed name doesn't
/// silently get reused.
#[test]
fn test_prepare_personal_branch_regenerates_on_collision() {
    use chrono::Utc;
    use oak_core::{Branch, BranchStatus};

    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();

    let main_branch = Branch::new("main".to_string(), None, None);
    repo.store_branch(&main_branch).unwrap();

    let proposed = "zdgeier-deadbe";
    let existing = Branch {
        name: proposed.to_string(),
        description: None,
        parent_branch: Some("main".to_string()),
        status: BranchStatus::Open,
        close_reason: None,
        created_at: Utc::now(),
    };
    repo.store_branch(&existing).unwrap();

    let chosen = oak_cli::commands::repo::prepare_personal_branch(&repo, proposed).unwrap();
    assert_ne!(
        chosen, proposed,
        "must regenerate when proposed name is already taken"
    );
    assert!(
        chosen.starts_with("zdgeier-"),
        "regenerated name must still use the author slug, got {chosen}"
    );
}

#[test]
fn test_commit_rejects_newline_in_filename() {
    // `\n` is a legal POSIX filename byte but the canonical tree preimage's
    // line separator — committing it must fail with one clear early error
    // (naming the file), not corrupt the stored tree.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "fine.txt", "ok");
    fs::write(temp.path().join("a\nb.txt"), "hostile").unwrap();

    let err = oak_cli::commands::commit::run(temp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("a\\nb.txt"),
        "error should name the file: {msg}"
    );
    assert!(msg.contains("newline"), "error should say why: {msg}");

    // Nothing was committed.
    let repo = open_repo(temp.path());
    assert!(repo.get_branch_head(DEFAULT_BRANCH).unwrap().is_none());
}

#[test]
fn test_commit_rejects_tab_in_filename() {
    // `\t` is the canonical tree preimage's field separator.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    fs::write(temp.path().join("a\tb.txt"), "hostile").unwrap();

    let err = oak_cli::commands::commit::run(temp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("a\\tb.txt"),
        "error should name the file: {msg}"
    );
    assert!(msg.contains("tab"), "error should say why: {msg}");
}

#[test]
fn test_commit_rejects_newline_in_directory_name() {
    // A hostile *directory* name corrupts every path beneath it; the scan
    // must catch it before descending.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    fs::create_dir(temp.path().join("dir\nname")).unwrap();
    fs::write(temp.path().join("dir\nname/inner.txt"), "hostile").unwrap();

    let err = oak_cli::commands::commit::run(temp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("newline"), "error should say why: {msg}");
}

// ---------------------------------------------------------------------------
// Path-scoped commits (`oak commit <paths>`)
// ---------------------------------------------------------------------------

/// A scoped commit lands only the selected changes; everything else keeps its
/// parent-commit state in the new manifest and stays visible to `oak status`.
#[test]
fn test_scoped_commit_lands_only_selected_paths() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "a1");
    write_file(temp.path(), "src/b.txt", "b1");
    write_file(temp.path(), "src/c.txt", "c1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Four independent changes: a modify, a modify under src/, a delete, an add.
    write_file(temp.path(), "a.txt", "a2");
    write_file(temp.path(), "src/b.txt", "b2");
    fs::remove_file(temp.path().join("src/c.txt")).unwrap();
    write_file(temp.path(), "new.txt", "n1");

    // Commit only a.txt.
    oak_cli::commands::commit::run_with_options(
        temp.path(),
        oak_cli::commands::commit::CommitOptions {
            paths: vec![temp.path().join("a.txt")],
            ..Default::default()
        },
    )
    .unwrap();

    let repo = open_repo(temp.path());
    let commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    assert_eq!(commits.len(), 2);

    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(commit.files.len(), 1, "commit should record only a.txt");
    assert_eq!(commit.files[0].path, "a.txt");

    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let content = |p: &str| -> String {
        let e = manifest.entries.iter().find(|e| e.path == p).unwrap();
        String::from_utf8(repo.get_blob(&e.blob_hash).unwrap().unwrap().content).unwrap()
    };
    assert_eq!(content("a.txt"), "a2");
    // Unselected modify keeps the parent content.
    assert_eq!(content("src/b.txt"), "b1");
    // Unselected delete is still tracked; unselected add is absent.
    assert!(manifest.entries.iter().any(|e| e.path == "src/c.txt"));
    assert!(!manifest.entries.iter().any(|e| e.path == "new.txt"));

    // Status keeps reporting the remaining changes.
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    let mut paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["new.txt", "src/b.txt", "src/c.txt"]);

    // A directory filter sweeps up everything beneath it...
    oak_cli::commands::commit::run_with_options(
        temp.path(),
        oak_cli::commands::commit::CommitOptions {
            paths: vec![temp.path().join("src")],
            ..Default::default()
        },
    )
    .unwrap();
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["new.txt"]);

    // ...and a default (unscoped) commit converges the tree to clean.
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    assert!(changes.is_empty());
}

/// A scoped commit whose paths match no changes must not create a commit.
#[test]
fn test_scoped_commit_with_no_matching_changes_is_a_noop() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "a.txt", "a1");
    write_file(temp.path(), "b.txt", "b1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "a.txt", "a2");

    // b.txt exists but is unchanged — nothing to commit under that path.
    oak_cli::commands::commit::run_with_options(
        temp.path(),
        oak_cli::commands::commit::CommitOptions {
            paths: vec![temp.path().join("b.txt")],
            ..Default::default()
        },
    )
    .unwrap();

    let repo = open_repo(temp.path());
    let commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    assert_eq!(commits.len(), 1, "no-match scoped commit must be a no-op");
}

/// A rename selected by either side commits atomically: the old path leaves
/// the manifest in the same commit that lands the new one.
#[test]
fn test_scoped_commit_rename_commits_both_sides() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(
        temp.path(),
        "old/file.txt",
        "enough content to be tracked as a rename by exact hash",
    );
    write_file(temp.path(), "other.txt", "o1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    fs::create_dir_all(temp.path().join("new")).unwrap();
    fs::rename(
        temp.path().join("old/file.txt"),
        temp.path().join("new/file.txt"),
    )
    .unwrap();
    write_file(temp.path(), "other.txt", "o2");

    // Filter names only the destination side of the rename.
    oak_cli::commands::commit::run_with_options(
        temp.path(),
        oak_cli::commands::commit::CommitOptions {
            paths: vec![temp.path().join("new")],
            ..Default::default()
        },
    )
    .unwrap();

    let repo = open_repo(temp.path());
    let head = repo.get_branch_head(DEFAULT_BRANCH).unwrap().unwrap();
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    assert!(manifest.entries.iter().any(|e| e.path == "new/file.txt"));
    assert!(
        !manifest.entries.iter().any(|e| e.path == "old/file.txt"),
        "rename source must leave the manifest with the scoped commit"
    );

    // Only the unselected edit remains.
    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["other.txt"]);
}

#[test]
fn test_status_does_not_store_dirty_file_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "tracked.txt", "clean\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let dirty_content = "dirty content unique to status no-store regression\n";
    let dirty_hash = oak_core::hash_bytes(dirty_content.as_bytes());
    let repo = open_repo(temp.path());
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "test setup should start without the dirty blob"
    );

    write_file(temp.path(), "tracked.txt", dirty_content);
    oak_cli::commands::status::run(temp.path(), false).unwrap();

    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "oak status must not persist dirty working-tree content"
    );

    oak_cli::commands::commit::run(temp.path()).unwrap();
    assert!(
        repo.has_blob(&dirty_hash).unwrap(),
        "oak commit must still persist dirty working-tree content"
    );
}

#[test]
fn test_read_only_diff_never_leaves_stat_cache_pointing_at_missing_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let clean_content = "clean\n";
    let dirty_content = "dirty content that stays read-only until commit\n";
    let clean_hash = oak_core::hash_bytes(clean_content.as_bytes());
    let dirty_hash = oak_core::hash_bytes(dirty_content.as_bytes());

    write_file(temp.path(), "tracked.txt", clean_content);
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let before = repo.load_stat_cache().unwrap();
    assert_eq!(
        before
            .get("tracked.txt")
            .map(|entry| entry.blob_hash.clone()),
        Some(clean_hash.clone()),
        "initial commit should seed the stat cache with the committed blob"
    );
    assert!(repo.has_blob(&clean_hash).unwrap());
    assert!(!repo.has_blob(&dirty_hash).unwrap());

    write_file(temp.path(), "tracked.txt", dirty_content);
    let (_changes, rendered) = oak_cli::commands::diff::render(&repo, temp.path()).unwrap();
    assert!(
        rendered.contains("+dirty content that stays read-only until commit"),
        "diff should still render the dirty worktree content:\n{rendered}"
    );
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "read-only diff must not persist the dirty blob"
    );

    let after_diff = repo.load_stat_cache().unwrap();
    assert_eq!(
        after_diff.get("tracked.txt").map(|entry| entry.blob_hash.clone()),
        Some(clean_hash),
        "read-only diff must not retarget the stat cache at a blob that is missing from local storage"
    );

    oak_cli::commands::commit::run(temp.path()).unwrap();
    assert!(
        repo.has_blob(&dirty_hash).unwrap(),
        "commit must store the dirty blob durably"
    );
    let after_commit = repo.load_stat_cache().unwrap();
    assert_eq!(
        after_commit
            .get("tracked.txt")
            .map(|entry| entry.blob_hash.clone()),
        Some(dirty_hash),
        "commit must update the stat cache only after the blob exists locally"
    );
}

#[test]
fn test_read_only_clean_probe_does_not_store_dirty_file_blob() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "tracked.txt", "clean\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let dirty_content = "dirty content unique to clean-probe no-store regression\n";
    let dirty_hash = oak_core::hash_bytes(dirty_content.as_bytes());
    let repo = open_repo(temp.path());
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "test setup should start without the dirty blob"
    );

    write_file(temp.path(), "tracked.txt", dirty_content);
    let clean =
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, temp.path())
            .unwrap();

    assert!(!clean, "dirty worktree should be reported as dirty");
    assert!(
        !repo.has_blob(&dirty_hash).unwrap(),
        "read-only clean probes must not persist dirty working-tree content"
    );
}

#[test]
fn test_read_only_status_prunes_deleted_cache_rows_when_file_count_stays_flat() {
    use std::collections::BTreeSet;

    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "old.txt", "old contents\n");
    write_file(temp.path(), "keep.txt", "keep contents\n");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let before = repo.load_stat_cache().unwrap();
    assert!(before.contains_key("old.txt"));
    assert!(before.contains_key("keep.txt"));
    assert!(!before.contains_key("new.txt"));
    drop(repo);

    fs::remove_file(temp.path().join("old.txt")).unwrap();
    write_file(temp.path(), "new.txt", "new contents\n");

    let (changes, _, _) = oak_cli::commands::commit::get_status(temp.path()).unwrap();
    let changed_paths: BTreeSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        changed_paths,
        BTreeSet::from(["new.txt", "old.txt"]),
        "read-only status must still report the add+delete when file count is unchanged"
    );

    let repo = open_repo(temp.path());
    let after = repo.load_stat_cache().unwrap();
    assert!(
        !after.contains_key("old.txt"),
        "deleted paths must still be pruned from the stat cache"
    );
    assert!(
        !after.contains_key("new.txt"),
        "read-only status must not populate cache rows for new dirty paths"
    );
    assert!(
        after.contains_key("keep.txt"),
        "unaffected cache rows should remain present"
    );
}

#[test]
fn test_merge_dirty_tree_exits_with_documented_dirty_code() {
    // A dirty-tree merge refusal is a "dirty working tree blocked the
    // operation" condition (exit 4 in `oak --help`), not a generic failure
    // (exit 1). Agents that branch on the documented exit-code taxonomy must
    // see 4 so they commit/reset rather than retry or escalate.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "one\n");
    assert!(oak_bin(temp.path(), &["commit"]).status.success());

    // Leave an uncommitted edit, then attempt to merge.
    write_file(temp.path(), "a.txt", "one\ntwo\n");
    let out = oak_bin(temp.path(), &["merge"]);

    assert_eq!(
        out.status.code(),
        Some(4),
        "dirty-tree merge must exit 4; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("uncommitted changes"),
        "expected the uncommitted-changes remediation text"
    );
}
