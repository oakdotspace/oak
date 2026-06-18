//! Tests that oak commands work when invoked from a subdirectory of the repo.
//!
//! `resolve()` walks up the directory tree from the given path, the same way
//! `git` discovers its repository. These tests exercise that walk and confirm
//! that path-taking commands interpret relative paths as cwd-relative when the
//! cwd is below the repo root.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use oak_cli::resolve::Backend;
use oak_core::OakError;
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

/// Pin the author component so assertions don't depend on `OAK_AUTHOR`/`USER`.
/// Production `oak init` creates a personal branch named `<author>-<rand6hex>`,
/// so the resulting branch is `tester-<rand6hex>` (not just `tester`) — tests
/// query the repo for the actual name rather than assuming it.
const TEST_AUTHOR: &str = "tester";

fn init_repo(dir: &Path) {
    // Tests run in parallel and env vars are process-wide; setting this
    // every call is a no-op after the first and stays consistent across the
    // whole run.
    unsafe { std::env::set_var("OAK_AUTHOR", TEST_AUTHOR) };
    // Non-interactive: don't fire the optional setup prompts when a developer
    // runs the suite from a terminal (stdin would otherwise be a TTY).
    oak_cli::commands::init::run(dir, false).unwrap();
}

/// The personal branch `oak init` created for this repo (`<author>-<rand6hex>`).
fn current_branch(dir: &Path) -> String {
    open_repo(dir).get_current_branch_name().unwrap().unwrap()
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

fn git(args: &[&str], dir: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git should run");
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn init_git_repo_with_commit(dir: &Path) {
    git(&["init", "-q"], dir);
    write_file(dir, "tracked.txt", "base\n");
    git(&["add", "tracked.txt"], dir);
    git(
        &[
            "-c",
            "user.name=Oak Test",
            "-c",
            "user.email=oak-test@example.com",
            "commit",
            "-q",
            "-m",
            "initial",
        ],
        dir,
    );
}

/// Make `dir/sub/nested` and return the deepest path. Mirrors a typical
/// "user is editing inside a deeply-nested module" scenario.
fn make_nested(root: &Path, segments: &[&str]) -> PathBuf {
    let mut p = root.to_path_buf();
    for s in segments {
        p.push(s);
    }
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn resolve_walks_up_to_find_oak_dir() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let nested = make_nested(temp.path(), &["a", "b", "c"]);

    let ctx = oak_cli::resolve::resolve(&nested).unwrap();

    // work_tree should be the canonicalized repo root, not the nested path.
    let expected = fs::canonicalize(temp.path()).unwrap();
    assert_eq!(ctx.work_tree, expected);
    assert_eq!(ctx.oak_dir, expected.join(".oak"));
}

#[test]
fn resolve_returns_repo_not_found_outside_any_repo() {
    let temp = TempDir::new().unwrap();
    // No init_repo — this directory is not a repo and has no ancestor repo
    // (TempDir lives under the system temp dir, which is not an oak repo).
    let result = oak_cli::resolve::resolve(temp.path());
    assert!(matches!(result, Err(OakError::RepoNotFound)));
}

#[test]
fn resolve_picks_nearest_oak_when_nested_repos_exist() {
    // outer/.oak ; outer/inner/.oak — invoking from outer/inner/sub should
    // return outer/inner, not outer. This is git's behavior too.
    let outer = TempDir::new().unwrap();
    init_repo(outer.path());
    let inner = outer.path().join("inner");
    fs::create_dir_all(&inner).unwrap();
    init_repo(&inner);
    let sub = make_nested(&inner, &["sub"]);

    let ctx = oak_cli::resolve::resolve(&sub).unwrap();

    let expected_inner = fs::canonicalize(&inner).unwrap();
    assert_eq!(ctx.work_tree, expected_inner);
}

#[test]
fn resolve_git_repo_does_not_create_oak_sidecar() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();

    let ctx = oak_cli::resolve::resolve(temp.path()).unwrap();

    assert!(
        matches!(ctx.backend, Backend::Git { .. }),
        "expected git backend, got {:?}",
        ctx.backend
    );
    assert_eq!(
        ctx.oak_dir,
        fs::canonicalize(temp.path()).unwrap().join(".git/oak")
    );
    assert!(
        !ctx.oak_dir.exists(),
        "Git-backed read-only resolution must not create Oak sidecar state"
    );
}

#[test]
fn git_backed_read_commands_do_not_create_oak_sidecar() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_commit(temp.path());
    let sidecar = temp.path().join(".git/oak");

    oak_cli::commands::status::run(temp.path(), false).unwrap();
    oak_cli::commands::diff::run(temp.path(), &[], false, false).unwrap();
    oak_cli::commands::log::run(temp.path(), Some(1), false, true, &[], None).unwrap();

    assert!(
        !sidecar.exists(),
        "Git-backed read commands must not create Oak sidecar state"
    );
}

#[test]
fn git_backed_commit_creates_oak_sidecar_on_write() {
    let temp = TempDir::new().unwrap();
    init_git_repo_with_commit(temp.path());
    let sidecar = temp.path().join(".git/oak");
    assert!(!sidecar.exists());
    write_file(temp.path(), "tracked.txt", "changed by oak\n");

    oak_cli::commands::commit::run(temp.path()).unwrap();

    assert!(
        sidecar.is_dir(),
        "Git-backed writes must create the Oak sidecar on demand"
    );
}

#[test]
fn commit_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "src/lib.rs", "fn main() {}");
    let src = temp.path().join("src");

    // Invoking commit with a subdirectory path should still find the repo
    // and commit the entire repo's working tree.
    oak_cli::commands::commit::run(&src).unwrap();

    let repo = open_repo(temp.path());
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let commits = repo.get_commits_for_branch(&branch).unwrap();
    assert_eq!(commits.len(), 1);
    // Local commits no longer carry messages — branch descriptions are the
    // source of truth and the squash-merge to main is what gets a message.
    assert_eq!(commits[0].message, None);

    // The committed manifest should contain the file at its full repo-relative path.
    let manifest = repo
        .get_manifest(&commits[0].manifest_hash)
        .unwrap()
        .unwrap();
    assert!(
        manifest.entries.iter().any(|e| e.path == "src/lib.rs"),
        "expected src/lib.rs in manifest, got: {:?}",
        manifest.entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
}

#[test]
fn status_log_diff_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "a.txt", "hello");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let nested = make_nested(temp.path(), &["deep", "dir"]);

    // None of these should fail (or panic) when invoked from a subdirectory.
    oak_cli::commands::status::run(&nested, false).unwrap();
    oak_cli::commands::log::run(&nested, None, false, false, &[], None).unwrap();
    oak_cli::commands::diff::run(&nested, &[], false, false).unwrap();
}

#[test]
fn branch_operations_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    // The personal branch `oak init` created (`tester-<rand6hex>`).
    let personal_branch = current_branch(temp.path());
    write_file(temp.path(), "f.txt", "x");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let sub = make_nested(temp.path(), &["pkg"]);

    oak_cli::commands::branch::new_branch(&sub, "feature", Some("from subdir"), None, None)
        .unwrap();

    let repo = open_repo(temp.path());
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_eq!(current, "feature");

    // Under the flat model `new_branch` no longer seeds the new branch at the
    // current HEAD, so `f.txt` (committed on the personal branch) shows up as
    // uncommitted on the empty `feature` branch. Commit it from the subdir —
    // which also exercises commit-path resolution from a nested cwd — so the
    // working tree is consistent and the switch below isn't blocked by
    // uncommitted changes.
    oak_cli::commands::commit::run(&sub).unwrap();

    // Switch back from a subdir. `main` doesn't exist locally under the
    // new model — switch to the personal branch the helper created.
    oak_cli::commands::switch::run(&sub, Some(&personal_branch), false).unwrap();
    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().unwrap(),
        personal_branch
    );
}

#[test]
fn reset_with_relative_path_from_subdirectory() {
    // `oak reset foo.txt` from a subdir should reset the file in that subdir,
    // not a same-named file at the repo root.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "root.txt", "root v1");
    write_file(temp.path(), "sub/inner.txt", "inner v1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    // Modify both files.
    write_file(temp.path(), "root.txt", "root v2");
    write_file(temp.path(), "sub/inner.txt", "inner v2");

    let sub = temp.path().join("sub");
    // Reset just inner.txt by passing it as cwd-relative from `sub/`.
    oak_cli::commands::reset::run(&sub, Some(Path::new("inner.txt")), true).unwrap();

    // inner.txt is back to v1, root.txt remains v2 (untouched).
    assert_eq!(
        fs::read_to_string(temp.path().join("sub/inner.txt")).unwrap(),
        "inner v1"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("root.txt")).unwrap(),
        "root v2"
    );
}

#[test]
fn restore_with_relative_path_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    write_file(temp.path(), "root.txt", "root v1");
    write_file(temp.path(), "sub/inner.txt", "inner v1");
    oak_cli::commands::commit::run(temp.path()).unwrap();

    write_file(temp.path(), "root.txt", "root v2");
    write_file(temp.path(), "sub/inner.txt", "inner v2");

    let sub = temp.path().join("sub");
    oak_cli::commands::restore::run(&sub, &[PathBuf::from("inner.txt")], None, true).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("sub/inner.txt")).unwrap(),
        "inner v1"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("root.txt")).unwrap(),
        "root v2"
    );
}

#[test]
fn init_does_not_walk_up() {
    // Initing inside an existing repo should create a nested repo at the
    // requested path, not error or surface the outer repo's .oak.
    let temp = TempDir::new().unwrap();
    init_repo(temp.path());
    let nested = make_nested(temp.path(), &["nested"]);
    init_repo(&nested);

    assert!(nested.join(".oak").exists());
    // resolve from the nested dir picks the nested repo, not the outer one.
    let ctx = oak_cli::resolve::resolve(&nested).unwrap();
    assert_eq!(ctx.work_tree, fs::canonicalize(&nested).unwrap());
}
