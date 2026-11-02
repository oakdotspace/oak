//! Case-only renames between snapshots (`Case.txt` → `case.txt`, `Dir/` →
//! `dir/`). On a case-insensitive filesystem, the macOS default, the two
//! spellings are one directory entry, and every working-tree move used to
//! lose the file: `oak switch`'s whole-tree rewrite swept the old spelling
//! (the only one on disk) as untracked, and the tree diff behind `oak merge`
//! and `oak pull` deleted the dropped path, which was the file it had just
//! written. A conflicted merge left the old spellings on disk until
//! `--continue`, which then deleted them. The assertions hold on a
//! case-sensitive filesystem too, where the spellings are two files and the
//! dropped one is deleted.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use oak_core::{FileMode, Hash, ManifestEntry, MetadataKey, OakError};
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

/// The tracked files before the rename.
const OLD: [(&str, &str); 3] = [
    ("Case.txt", "x\n"),
    ("Same.txt", "s\n"),
    ("Dir/in.txt", "i\n"),
];
/// The same files renamed by case alone, `case.txt` with new content.
const NEW: [(&str, &str); 3] = [
    ("case.txt", "y\n"),
    ("same.txt", "s\n"),
    ("dir/in.txt", "i\n"),
];

fn init_repo(dir: &Path) -> SqliteRepository {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoName, "case-renames-test")
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

fn entries(repo: &SqliteRepository, files: &[(&str, &str)]) -> Vec<ManifestEntry> {
    files
        .iter()
        .map(|(path, content)| ManifestEntry {
            path: path.to_string(),
            blob_hash: repo.put_blob(content.as_bytes().to_vec()).unwrap(),
            mode: FileMode::Regular,
        })
        .collect()
}

fn commit(
    repo: &SqliteRepository,
    branch: &str,
    parent: Option<&Hash>,
    files: &[(&str, &str)],
) -> Hash {
    let root = repo.put_manifest(entries(repo, files)).unwrap();
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

/// Every file under `dir` except `.oak`, sorted, as spelled on disk, with its
/// content.
fn on_disk(dir: &Path) -> Vec<(String, String)> {
    fn walk(path: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            if rel == ".oak" {
                continue;
            }
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(&path, root, out);
            } else {
                out.push((rel, fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn expected(files: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = files
        .iter()
        .map(|(path, content)| (path.to_string(), content.to_string()))
        .collect();
    out.sort();
    out
}

/// The stat cache holds exactly `files`, under their spelling, describing
/// them as they are on disk now (a rename changes ctime).
#[cfg(unix)]
fn assert_stat_cache(dir: &Path, files: &[(&str, &str)]) {
    use std::os::unix::fs::MetadataExt;
    let cache = open_repo(dir).load_stat_cache().unwrap();
    let mut cached: Vec<&str> = cache.keys().map(String::as_str).collect();
    cached.sort_unstable();
    let mut paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
    paths.sort_unstable();
    assert_eq!(cached, paths);
    for (path, content) in files {
        let row = &cache[*path];
        assert_eq!(
            row.blob_hash,
            oak_core::hash_bytes(content.as_bytes()),
            "{path}"
        );
        let md = fs::metadata(dir.join(path)).unwrap();
        let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
        let ctime_ns = md.ctime() * 1_000_000_000 + md.ctime_nsec();
        assert_eq!(
            (row.size, row.mtime_ns, row.ctime_ns),
            (md.len() as i64, mtime_ns, ctime_ns),
            "{path}"
        );
    }
}

#[cfg(not(unix))]
fn assert_stat_cache(_dir: &Path, _files: &[(&str, &str)]) {}

fn assert_clean(dir: &Path) {
    let (changes, _, _) = oak_cli::commands::commit::get_status(dir).unwrap();
    let changes: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    assert!(changes.is_empty(), "unexpected changes: {changes:?}");
}

/// `oak switch` rewrites the whole tree: switching to a branch that renamed
/// the files by case, and back, leaves each branch's files under its own
/// spelling.
#[test]
fn switch_across_a_case_only_rename_keeps_the_files() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base = commit(&repo, "dev", None, &OLD);
    commit(&repo, "feature", Some(&base), &NEW);
    check_out_feature(temp.path(), &repo);
    drop(repo);
    assert_eq!(on_disk(temp.path()), expected(&NEW));

    oak_cli::commands::switch::run(temp.path(), Some("dev"), false).unwrap();
    assert_eq!(on_disk(temp.path()), expected(&OLD));
    assert_clean(temp.path());

    oak_cli::commands::switch::run(temp.path(), Some("feature"), false).unwrap();
    assert_eq!(on_disk(temp.path()), expected(&NEW));
    assert_clean(temp.path());
}

/// `oak pull`'s parent sync moves the tree by the diff, deleting the paths the
/// merge dropped: the parent's case-only rename lands under its spelling.
#[tokio::test(flavor = "current_thread")]
async fn sync_landing_a_case_only_rename_keeps_the_files() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base = commit(&repo, "dev", None, &OLD);
    let feature_files = [OLD.as_slice(), &[("feature.txt", "f\n")]].concat();
    commit(&repo, "feature", Some(&base), &feature_files);
    commit(&repo, "dev", Some(&base), &NEW);
    check_out_feature(temp.path(), &repo);
    drop(repo);

    oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap();

    let merged = [NEW.as_slice(), &[("feature.txt", "f\n")]].concat();
    assert_eq!(on_disk(temp.path()), expected(&merged));
    assert_clean(temp.path());
}

/// `oak merge` into a local parent that renamed the files by case: the
/// working tree moves from the branch's tree to the merge by the diff.
#[tokio::test(flavor = "current_thread")]
async fn merge_landing_on_a_case_only_rename_keeps_the_files() {
    let temp = TempDir::new().unwrap();
    let repo = init_repo(temp.path());
    let base = commit(&repo, "dev", None, &OLD);
    let feature_files = [OLD.as_slice(), &[("feature.txt", "f\n")]].concat();
    commit(&repo, "feature", Some(&base), &feature_files);
    commit(&repo, "dev", Some(&base), &NEW);
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
    let merged = [NEW.as_slice(), &[("feature.txt", "f\n")]].concat();
    assert_eq!(on_disk(temp.path()), expected(&merged));
    assert_clean(temp.path());
}

/// Run the `oak` binary in `dir`, fail unless it succeeds, and return what
/// it printed.
fn oak(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_AUTHOR", "tester")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "oak {args:?} failed:\n{printed}");
    printed
}

/// `files` plus `conflict.txt` holding `content`.
fn with_conflict<'a>(files: &[(&'a str, &'a str)], content: &'a str) -> Vec<(&'a str, &'a str)> {
    [files, &[("conflict.txt", content)]].concat()
}

/// `dev` renamed the files by case, and both sides edited `conflict.txt`, so
/// merging them stops for a resolution.
fn conflicted_case_rename(dir: &Path) {
    let repo = init_repo(dir);
    let base = commit(&repo, "dev", None, &with_conflict(&OLD, "base\n"));
    commit(
        &repo,
        "feature",
        Some(&base),
        &with_conflict(&OLD, "feature\n"),
    );
    commit(&repo, "dev", Some(&base), &with_conflict(&NEW, "dev\n"));
    check_out_feature(dir, &repo);
}

/// A conflicted `oak merge` into a parent that renamed the files by case. The
/// conflict write-out deletes nothing, so the merged files land in the old
/// spellings' entries and keep those names; `--continue` then removes the
/// paths the merge dropped, which are the merged files under another
/// spelling. They are renamed instead, `case.txt` (new content) included.
#[tokio::test(flavor = "current_thread")]
async fn merge_continue_keeps_case_only_renames() {
    let temp = TempDir::new().unwrap();
    conflicted_case_rename(temp.path());

    let err = oak_cli::commands::merge::run(temp.path(), false, false, None, None, false, false)
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");
    fs::write(temp.path().join("conflict.txt"), "resolved\n").unwrap();
    let printed = oak(temp.path(), &["merge", "--continue"]);
    // Nothing is reported as a local change: each renamed file is compared
    // with the merge under its final spelling.
    assert!(!printed.contains("local changes unrelated"), "{printed}");

    let repo = open_repo(temp.path());
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("dev")
    );
    let merged = with_conflict(&NEW, "resolved\n");
    assert_eq!(on_disk(temp.path()), expected(&merged));
    assert_stat_cache(temp.path(), &merged);
    assert_clean(temp.path());
}

/// `oak pull --continue` after the parent sync stopped on a conflict, with
/// the parent's case-only rename in the merge.
#[tokio::test(flavor = "current_thread")]
async fn sync_continue_keeps_case_only_renames() {
    let temp = TempDir::new().unwrap();
    conflicted_case_rename(temp.path());

    let err = oak_cli::commands::sync::sync_from_parent(temp.path())
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");
    fs::write(temp.path().join("conflict.txt"), "resolved\n").unwrap();
    let printed = oak(temp.path(), &["pull", "--continue"]);
    assert!(!printed.contains("local changes unrelated"), "{printed}");

    let merged = with_conflict(&NEW, "resolved\n");
    assert_eq!(on_disk(temp.path()), expected(&merged));
    assert_stat_cache(temp.path(), &merged);
    assert_clean(temp.path());
}
