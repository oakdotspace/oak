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

    oak_cli::commands::merge::run(temp.path(), false, false, None, false, false)
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

    let err = oak_cli::commands::merge::run(temp.path(), false, false, None, false, false)
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

// fb-661: the same renames between Unicode normalization forms. APFS and
// HFS+ fold NFC `café.txt` and NFD `café.txt` into one entry (case-sensitive
// APFS too), which no lowercasing pairs; elsewhere they are two files.

/// The tracked files before the rename, NFC (precomposed).
const NFC_OLD: [(&str, &str); 3] = [
    ("caf\u{e9}.txt", "x\n"),
    ("na\u{ef}ve.txt", "s\n"),
    ("r\u{e9}sum\u{e9}/in.txt", "i\n"),
];
/// The same files renamed to NFD (decomposed), `café.txt` with new content.
const NFD_NEW: [(&str, &str); 3] = [
    ("cafe\u{301}.txt", "y\n"),
    ("nai\u{308}ve.txt", "s\n"),
    ("re\u{301}sume\u{301}/in.txt", "i\n"),
];

/// The tree expected after moving from `old` to `new` (paired by index), as
/// spelled on this filesystem: `new`, except where it folds NFC and NFD but
/// would not respell in place (case-sensitive APFS), where each renamed path
/// keeps its `old` spelling, with `new`'s content.
fn expected_spelled(
    dir: &Path,
    old: &[(&str, &str)],
    new: &[(&'static str, &'static str)],
    extra: &[(&'static str, &'static str)],
) -> Vec<(String, String)> {
    let probe = dir.join("respell-caf\u{e9}");
    fs::write(&probe, "").unwrap();
    let folds = dir.join("respell-cafe\u{301}").exists();
    let _ = fs::rename(&probe, dir.join("respell-cafe\u{301}"));
    let respells = fs::read_dir(dir)
        .unwrap()
        .any(|e| e.unwrap().file_name() == "respell-cafe\u{301}");
    for name in ["respell-caf\u{e9}", "respell-cafe\u{301}"] {
        let _ = fs::remove_file(dir.join(name));
    }
    let mut files: Vec<(&str, &str)> = new
        .iter()
        .zip(old)
        .map(|((path, content), (was, _))| {
            if folds && !respells {
                (*was, *content)
            } else {
                (*path, *content)
            }
        })
        .collect();
    files.extend_from_slice(extra);
    expected(&files)
}

/// `dev` renamed the files from `old` to `new` spelling, and both sides
/// edited `conflict.txt`, so merging them stops for a resolution.
fn conflicted_rename(dir: &Path, old: &[(&str, &str)], new: &[(&str, &str)]) {
    let repo = init_repo(dir);
    let base = commit(&repo, "dev", None, &with_conflict(old, "base\n"));
    commit(
        &repo,
        "feature",
        Some(&base),
        &with_conflict(old, "feature\n"),
    );
    commit(&repo, "dev", Some(&base), &with_conflict(new, "dev\n"));
    check_out_feature(dir, &repo);
}

#[test]
fn switch_across_a_normalization_rename_keeps_the_files() {
    for (old, new) in [(NFC_OLD, NFD_NEW), (NFD_NEW, NFC_OLD)] {
        let temp = TempDir::new().unwrap();
        let repo = init_repo(temp.path());
        let base = commit(&repo, "dev", None, &old);
        commit(&repo, "feature", Some(&base), &new);
        check_out_feature(temp.path(), &repo);
        drop(repo);
        assert_eq!(on_disk(temp.path()), expected(&new));

        oak_cli::commands::switch::run(temp.path(), Some("dev"), false).unwrap();
        let want = expected_spelled(temp.path(), &new, &old, &[]);
        assert_eq!(on_disk(temp.path()), want);
        if want == expected(&old) {
            assert_clean(temp.path());
        }
        if want != expected(&old) {
            continue; // the old spelling stays; status reports the renames
        }

        oak_cli::commands::switch::run(temp.path(), Some("feature"), false).unwrap();
        assert_eq!(on_disk(temp.path()), expected(&new));
        assert_clean(temp.path());
    }
}

/// `oak merge --continue` and `oak pull --continue` remove the dropped
/// paths themselves: a conflicted merge with the parent's normalization
/// rename leaves exactly the merge, under the parent's spelling.
#[tokio::test(flavor = "current_thread")]
async fn continue_keeps_normalization_renames() {
    for (old, new) in [(NFC_OLD, NFD_NEW), (NFD_NEW, NFC_OLD)] {
        for sync in [false, true] {
            let temp = TempDir::new().unwrap();
            conflicted_rename(temp.path(), &old, &new);
            let err = if sync {
                oak_cli::commands::sync::sync_from_parent(temp.path())
                    .await
                    .unwrap_err()
            } else {
                oak_cli::commands::merge::run(temp.path(), false, false, None, false, false)
                    .await
                    .unwrap_err()
            };
            assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");
            fs::write(temp.path().join("conflict.txt"), "resolved\n").unwrap();
            let args: &[&str] = if sync {
                &["pull", "--continue"]
            } else {
                &["merge", "--continue"]
            };
            let printed = oak(temp.path(), args);

            let merged = with_conflict(&new, "resolved\n");
            let want = expected_spelled(temp.path(), &old, &new, &[("conflict.txt", "resolved\n")]);
            assert_eq!(on_disk(temp.path()), want, "sync={sync}");
            if want == expected(&merged) {
                assert!(!printed.contains("local changes unrelated"), "{printed}");
                assert_stat_cache(temp.path(), &merged);
                assert_clean(temp.path());
            } else {
                // Kept under the old spelling: said so, and reported as
                // local changes rather than folded into the merge.
                assert!(printed.contains("keeps its spelling on disk"), "{printed}");
            }
        }
    }
}

/// Run the `oak` binary in `dir` and return its exit code and stdout, which
/// for `--json` is one document (parsed from its first `{`).
fn oak_json(dir: &Path, args: &[&str]) -> (i32, serde_json::Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_AUTHOR", "tester")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let doc = stdout
        .find('{')
        .and_then(|at| serde_json::from_str(stdout[at..].trim()).ok())
        .unwrap_or_else(|| panic!("oak {args:?}: no JSON document:\n{stdout}{stderr}"));
    (output.status.code().unwrap_or(-1), doc, stderr)
}

/// Whether this filesystem folds NFC and NFD into one name but will not
/// respell an entry in place (case-sensitive APFS): the case where an update
/// keeps the old spelling. Probed, not assumed from the platform.
fn keeps_old_spelling(dir: &Path) -> bool {
    let probe = dir.join("probe-caf\u{e9}");
    fs::write(&probe, "").unwrap();
    let folds = dir.join("probe-cafe\u{301}").exists();
    let _ = fs::rename(&probe, dir.join("probe-cafe\u{301}"));
    let respells = fs::read_dir(dir)
        .unwrap()
        .any(|e| e.unwrap().file_name() == "probe-cafe\u{301}");
    for name in ["probe-caf\u{e9}", "probe-cafe\u{301}"] {
        let _ = fs::remove_file(dir.join(name));
    }
    folds && !respells
}

fn kept_pairs(doc: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let list = doc.get("kept_spellings")?.as_array()?;
    let mut pairs: Vec<(String, String)> = list
        .iter()
        .map(|kept| {
            (
                kept["on_disk"].as_str().unwrap().to_string(),
                kept["tracked"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    pairs.sort();
    Some(pairs)
}

/// fb-661 follow-up. Where the filesystem keeps a file's old spelling when
/// a branch respelled it (NFC → NFD on case-sensitive APFS), the update's
/// receipt (`oak switch --json` here; `pull --continue` below) and `oak status
/// --json` list it as `kept_spellings`, and a plain `oak commit` refuses
/// rather than record the old spelling back. Everywhere else no field
/// appears. A user's own rename — here case-only, which on a folding volume
/// looks the same on disk — is never listed.
#[test]
fn kept_spellings_are_reported_and_never_committed_back() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    let keeps = keeps_old_spelling(dir);
    let repo = init_repo(dir);
    let plain = [("plain.txt", "p\n")];
    let base = commit(&repo, "dev", None, &[NFC_OLD.as_slice(), &plain].concat());
    commit(
        &repo,
        "feature",
        Some(&base),
        &[NFC_OLD.as_slice(), &plain].concat(),
    );
    commit(
        &repo,
        "dev",
        Some(&base),
        &[NFD_NEW.as_slice(), &plain].concat(),
    );
    check_out_feature(dir, &repo);
    drop(repo);

    let (code, receipt, _) = oak_json(dir, &["switch", "dev", "--json"]);
    assert_eq!(code, 0, "{receipt}");
    let want: Vec<(String, String)> = {
        let mut pairs: Vec<(String, String)> = NFC_OLD
            .iter()
            .zip(&NFD_NEW)
            .map(|((was, _), (now, _))| (was.to_string(), now.to_string()))
            .collect();
        pairs.sort();
        pairs
    };
    let (_, status, _) = oak_json(dir, &["status", "--json"]);
    if keeps {
        assert_eq!(kept_pairs(&receipt), Some(want.clone()), "{receipt}");
        assert_eq!(kept_pairs(&status), Some(want.clone()), "{status}");
        let human = oak(dir, &["status"]);
        assert!(human.contains("kept by an update"), "{human}");
        assert!(human.contains(".respell"), "{human}");

        // A whole-tree commit would record NFC back over the branch's NFD.
        let (code, refusal, _) = oak_json(dir, &["commit", "--json"]);
        assert_ne!(code, 0, "{refusal}");
        assert_eq!(refusal["error"]["code"], "dirty_worktree", "{refusal}");
        assert_eq!(kept_pairs(&refusal), Some(want.clone()), "{refusal}");
        let (_, after, _) = oak_json(dir, &["status", "--json"]);
        assert_eq!(after["head"], status["head"], "nothing committed");

        // The documented way out: a two-step mv takes the tracked spelling
        // (of the directory, for the file inside `résumé/`).
        for (on_disk, tracked) in [
            ("caf\u{e9}.txt", "cafe\u{301}.txt"),
            ("na\u{ef}ve.txt", "nai\u{308}ve.txt"),
            ("r\u{e9}sum\u{e9}", "re\u{301}sume\u{301}"),
        ] {
            let step = format!("{on_disk}.respell");
            fs::rename(dir.join(on_disk), dir.join(&step)).unwrap();
            fs::rename(dir.join(&step), dir.join(tracked)).unwrap();
        }
    } else {
        assert_eq!(kept_pairs(&receipt), None, "{receipt}");
        assert_eq!(kept_pairs(&status), None, "{status}");
    }
    assert_eq!(
        on_disk(dir),
        expected(&[NFD_NEW.as_slice(), &plain].concat())
    );
    assert_clean(dir);

    // The user's own case-only rename is a rename, listed nowhere as kept,
    // and a plain commit records it.
    fs::rename(dir.join("plain.txt"), dir.join("Plain.txt")).unwrap();
    let (_, status, _) = oak_json(dir, &["status", "--json"]);
    assert_eq!(kept_pairs(&status), None, "{status}");
    let renamed = status["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "Plain.txt" && c["old_path"] == "plain.txt");
    assert!(renamed, "{status}");
    let (code, receipt, _) = oak_json(dir, &["commit", "--json"]);
    assert_eq!(code, 0, "{receipt}");
    assert_eq!(receipt["committed"], true, "{receipt}");
    assert_eq!(kept_pairs(&receipt), None, "{receipt}");
}

/// `oak pull --continue --json` finishes a parent sync that respelled files:
/// its receipt lists the kept spellings exactly where the filesystem kept
/// them, and carries no such field elsewhere.
#[tokio::test(flavor = "current_thread")]
async fn pull_continue_receipt_lists_kept_spellings() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    let keeps = keeps_old_spelling(dir);
    conflicted_rename(dir, &NFC_OLD, &NFD_NEW);
    let err = oak_cli::commands::sync::sync_from_parent(dir)
        .await
        .unwrap_err();
    assert!(matches!(err, OakError::MergeConflict(1)), "got {err}");
    fs::write(dir.join("conflict.txt"), "resolved\n").unwrap();
    let (code, receipt, _) = oak_json(dir, &["pull", "--continue", "--json"]);
    assert_eq!(code, 0, "{receipt}");
    let kept = kept_pairs(&receipt);
    if keeps {
        let kept = kept.unwrap_or_else(|| panic!("no kept_spellings: {receipt}"));
        assert!(!kept.is_empty(), "{receipt}");
        for (on_disk, tracked) in &kept {
            assert!(NFC_OLD.iter().any(|(p, _)| p == on_disk), "{receipt}");
            assert!(NFD_NEW.iter().any(|(p, _)| p == tracked), "{receipt}");
        }
    } else {
        assert_eq!(kept, None, "{receipt}");
    }
}

/// Round-3 review M1: editing a kept file — the ordinary pull-then-edit flow
/// — turns its `renamed` into `added` (on-disk spelling) + `deleted`
/// (tracked spelling). It is still the filesystem's spelling, so status
/// still lists it and a whole-tree commit still refuses, pointing at the
/// two-step `mv` and never at a per-path commit inside the kept directory.
/// After the `mv`, the edits commit as plain modifications.
#[test]
fn edited_kept_spellings_still_refuse_whole_tree_commits() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    let keeps = keeps_old_spelling(dir);
    let repo = init_repo(dir);
    let base = commit(&repo, "dev", None, &NFC_OLD);
    commit(&repo, "feature", Some(&base), &NFC_OLD);
    commit(&repo, "dev", Some(&base), &NFD_NEW);
    check_out_feature(dir, &repo);
    drop(repo);
    let (code, receipt, _) = oak_json(dir, &["switch", "dev", "--json"]);
    assert_eq!(code, 0, "{receipt}");

    // Edit café.txt and résumé/in.txt under whatever spelling is on disk.
    let (cafe, in_txt) = if keeps {
        ("caf\u{e9}.txt", "r\u{e9}sum\u{e9}/in.txt")
    } else {
        ("cafe\u{301}.txt", "re\u{301}sume\u{301}/in.txt")
    };
    fs::write(dir.join(cafe), "edited\n").unwrap();
    fs::write(dir.join(in_txt), "edited too\n").unwrap();

    if keeps {
        let mut want: Vec<(String, String)> = NFC_OLD
            .iter()
            .zip(&NFD_NEW)
            .map(|((was, _), (now, _))| (was.to_string(), now.to_string()))
            .collect();
        want.sort();
        let (_, status, _) = oak_json(dir, &["status", "--json"]);
        assert_eq!(kept_pairs(&status), Some(want.clone()), "{status}");

        let (code, refusal, _) = oak_json(dir, &["commit", "--json"]);
        assert_ne!(code, 0, "{refusal}");
        assert_eq!(refusal["error"]["code"], "dirty_worktree", "{refusal}");
        assert_eq!(kept_pairs(&refusal), Some(want), "{refusal}");
        let message = refusal["error"]["message"].as_str().unwrap();
        assert!(message.contains(".respell"), "{message}");
        assert!(message.contains("outside"), "{message}");
        let (_, after, _) = oak_json(dir, &["status", "--json"]);
        assert_eq!(after["head"], status["head"], "nothing committed");

        for (on_disk, tracked) in [
            ("caf\u{e9}.txt", "cafe\u{301}.txt"),
            ("na\u{ef}ve.txt", "nai\u{308}ve.txt"),
            ("r\u{e9}sum\u{e9}", "re\u{301}sume\u{301}"),
        ] {
            let step = format!("{on_disk}.respell");
            fs::rename(dir.join(on_disk), dir.join(&step)).unwrap();
            fs::rename(dir.join(&step), dir.join(tracked)).unwrap();
        }
    }
    let (_, status, _) = oak_json(dir, &["status", "--json"]);
    assert_eq!(kept_pairs(&status), None, "{status}");
    let (code, receipt, _) = oak_json(dir, &["commit", "--json"]);
    assert_eq!(code, 0, "{receipt}");
    assert_eq!(receipt["committed"], true, "{receipt}");
    assert_eq!(
        receipt["change_counts"],
        serde_json::json!({"added": 0, "modified": 2, "deleted": 0, "renamed": 0}),
        "{receipt}"
    );
}
