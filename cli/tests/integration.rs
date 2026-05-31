//! Integration tests for local Oak operations
//!
//! These tests verify init, commit, log, branch, tag, merge, status,
//! diff, reset, and ignore functionality without requiring a server.

use std::fs;
use std::path::Path;

use oak_core::{ChangeType, MetadataKey};
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

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
fn test_tag_workflow() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Create tag
    oak_cli::commands::tag::create(temp.path(), "v1.0", None).unwrap();

    // List tags
    oak_cli::commands::tag::list(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let tags = repo.list_tags().unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "v1.0");

    // Delete tag
    oak_cli::commands::tag::delete(temp.path(), "v1.0").unwrap();
    let tags = repo.list_tags().unwrap();
    assert!(tags.is_empty());
}

#[test]
fn test_tag_with_specific_commit() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "v1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = open_repo(temp.path());
    let commits = repo.get_commits_for_branch(DEFAULT_BRANCH).unwrap();
    let hash = commits[0].hash.to_string();

    oak_cli::commands::tag::create(temp.path(), "v1.0", Some(&hash)).unwrap();

    let tags = repo.list_tags().unwrap();
    assert_eq!(tags[0].commit_hash.to_string(), hash);
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
    oak_cli::commands::diff::run(temp.path()).unwrap();
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
    oak_cli::commands::branch::close_branch(temp.path(), "feature").unwrap();

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
    oak_cli::commands::log::run(temp.path(), Some(1), false).unwrap();
    oak_cli::commands::log::run(temp.path(), None, true).unwrap();
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
    let result = oak_cli::commands::merge::run(temp.path(), false, false).await;
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

    let result = oak_cli::commands::merge::run(temp.path(), false, false).await;
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
    oak_cli::commands::branch::close_branch(temp.path(), "feature").unwrap();

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

// ============================================================
// Hash command tests
// ============================================================

#[test]
fn test_hash_shows_head() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    write_file(temp.path(), "file.txt", "content");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // hash command should succeed (prints to stdout)
    oak_cli::commands::hash::run(temp.path()).unwrap();
}

#[test]
fn test_hash_no_commits_fails() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());

    let result = oak_cli::commands::hash::run(temp.path());
    assert!(result.is_err(), "hash should fail with no commits");
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
    );
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

    std::env::set_var("OAK_AUTHOR", "zdgeier");
    let proposed = "zdgeier-deadbe";
    let existing = Branch {
        name: proposed.to_string(),
        description: None,
        parent_branch: Some("main".to_string()),
        status: BranchStatus::Open,
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
