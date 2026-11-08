//! `oak switch`, `oak switch -c`, `oak split` and `oak branch --from` guard
//! against a dirty tree with the tree-hash clean check instead of the status
//! pipeline. These tests pin what that buys and what it must keep:
//!
//! - the guard reads nothing of HEAD, so a command whose target doesn't
//!   need HEAD's data runs when HEAD's trees or blobs are missing locally
//!   (`oak status` still refuses that snapshot). The `legacy` tree these
//!   tests drop is on the path the move's tree diff walks, so the move falls
//!   back to rewriting the whole tree from the target, as it used to
//!   (`worktree_moves.rs` covers the diff itself);
//! - an edited or untracked file still refuses (or, for `oak switch -c`, is
//!   carried rather than overwritten);
//! - with the guard no longer failing on a missing HEAD blob, the commands
//!   that publish refs before moving the working tree check the blobs that
//!   move needs first, so they stop before anything moves.

use std::fs;
use std::path::Path;

use oak_cli::commands::switch::WorktreePolicy;
use oak_core::{FileMode, Hash, ManifestEntry, MetadataKey, OakError};
use oak_core::{Repository, SqliteRepository};
use tempfile::TempDir;

fn init_repo(dir: &Path) -> SqliteRepository {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoName, "clean-guards-test")
        .unwrap();
    // No remote: `main` is resolved from its local head, so every flow
    // stays offline.
    repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
        .unwrap();
    for name in ["feature", "topic"] {
        repo.store_branch(&oak_core::Branch::new(
            name.to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
    }
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

fn blob(repo: &SqliteRepository, content: &str) -> Hash {
    repo.put_blob(content.as_bytes().to_vec()).unwrap()
}

/// `main` holds eight directories of four files each. `feature`, checked
/// out, edits `d1/f0.txt` in one commit and adds `legacy/old.txt` — a
/// directory no other snapshot has — in a second.
struct Fixture {
    temp: TempDir,
    base: Hash,
    base_entries: Vec<ManifestEntry>,
    /// `feature`'s commits, oldest first.
    feature: [Hash; 2],
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let repo = init_repo(temp.path());
        let mut base_entries = Vec::new();
        for d in 0..8 {
            for f in 0..4 {
                let b = blob(&repo, &format!("d{d}f{f}\n"));
                base_entries.push(entry(&format!("d{d}/f{f}.txt"), &b));
            }
        }
        let base = commit(&repo, "main", None, base_entries.clone());
        let edited = with(&base_entries, "d1/f0.txt", &blob(&repo, "branch\n"));
        let first = commit(&repo, "feature", Some(&base), edited.clone());
        let second = commit(
            &repo,
            "feature",
            Some(&first),
            with(&edited, "legacy/old.txt", &blob(&repo, "legacy\n")),
        );
        repo.set_head(&second).unwrap();
        let manifest = repo
            .get_manifest(&repo.get_commit(&second).unwrap().unwrap().manifest_hash)
            .unwrap()
            .unwrap();
        let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
        oak_cli::commands::switch::update_working_dir(&lock, temp.path(), &repo, &manifest)
            .unwrap();
        Fixture {
            temp,
            base,
            base_entries,
            feature: [first, second],
        }
    }

    fn dir(&self) -> &Path {
        self.temp.path()
    }

    fn repo(&self) -> SqliteRepository {
        open_repo(self.dir())
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.dir().join(path)).unwrap()
    }

    /// Drop HEAD's `legacy` tree and blob from the local store, leaving the
    /// file on disk: data only HEAD has, which the commands under test never
    /// need. `oak status` reads HEAD whole, so it now refuses.
    fn forget_legacy(&self) {
        let repo = self.repo();
        let root = repo
            .get_commit(&self.feature[1])
            .unwrap()
            .unwrap()
            .manifest_hash;
        let tree = repo.subtree_at_path(&root, "legacy").unwrap().unwrap();
        let legacy = repo
            .get_manifest(&root)
            .unwrap()
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.path == "legacy/old.txt")
            .unwrap()
            .blob_hash;
        drop(repo);
        delete_rows(self.dir(), "trees", &tree);
        delete_rows(self.dir(), "blobs", &legacy);
        oak_cli::commands::commit::get_status(self.dir())
            .expect_err("status still requires HEAD complete locally");
    }

    /// Drop `main`'s blob for `d1/f0.txt`, which `feature` edits: a move
    /// from HEAD to a snapshot holding `main`'s version must write it.
    fn forget_reverted_blob(&self) {
        let reverted = self
            .base_entries
            .iter()
            .find(|e| e.path == "d1/f0.txt")
            .unwrap()
            .blob_hash
            .clone();
        delete_rows(self.dir(), "blobs", &reverted);
    }

    /// `topic`: two commits on `main`, editing `d2` then `d3`.
    fn topic(&self) -> [Hash; 2] {
        let repo = self.repo();
        let first = with(&self.base_entries, "d2/f0.txt", &blob(&repo, "topic 1\n"));
        let c1 = commit(&repo, "topic", Some(&self.base), first.clone());
        let c2 = commit(
            &repo,
            "topic",
            Some(&c1),
            with(&first, "d3/f0.txt", &blob(&repo, "topic 2\n")),
        );
        [c1, c2]
    }

    /// `other`: one commit on `main` editing `d2/f0.txt`.
    fn other(&self) -> Hash {
        let repo = self.repo();
        repo.store_branch(&oak_core::Branch::new(
            "other".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        let entries = with(&self.base_entries, "d2/f0.txt", &blob(&repo, "other\n"));
        commit(&repo, "other", Some(&self.base), entries)
    }

    /// The two kinds of uncommitted work every guard must see, each written
    /// into a fresh fixture.
    fn each_dirty(check: impl Fn(&Fixture, &str, &str)) {
        for (path, content) in [("scratch.txt", "untracked\n"), ("d3/f1.txt", "edited\n")] {
            let fx = Fixture::new();
            fs::write(fx.dir().join(path), content).unwrap();
            check(&fx, path, content);
        }
    }
}

fn delete_rows(dir: &Path, table: &str, hash: &Hash) {
    let db = rusqlite::Connection::open(dir.join(".oak/oak.db")).unwrap();
    let deleted = db
        .execute(
            &format!("DELETE FROM {table} WHERE hash = ?1"),
            [hash.as_str()],
        )
        .unwrap();
    assert_eq!(deleted, 1, "{table} row {hash} should exist");
}

/// Write a split plan outside the working tree (inside it, the plan file
/// would itself be untracked work).
fn plan_file(plans: &TempDir, text: &str) -> String {
    let path = plans.path().join("plan.txt");
    fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

fn current_branch(fx: &Fixture) -> Option<String> {
    fx.repo().get_current_branch_name().unwrap()
}

#[test]
fn switch_guard_proves_clean_without_reading_head() {
    let fx = Fixture::new();
    let other = fx.other();
    fx.forget_legacy();

    oak_cli::commands::switch::run(fx.dir(), Some("other"), false).unwrap();

    assert_eq!(current_branch(&fx).as_deref(), Some("other"));
    assert_eq!(fx.repo().get_head().unwrap(), Some(other));
    assert_eq!(fx.read("d2/f0.txt"), "other\n");
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.dir().join("legacy").exists());
}

#[test]
fn switch_guard_refuses_untracked_and_edited_files() {
    Fixture::each_dirty(|fx, path, content| {
        fx.other();
        let err = oak_cli::commands::switch::run(fx.dir(), Some("other"), false)
            .expect_err("a dirty tree refuses the switch");
        assert!(matches!(err, OakError::UncommittedChanges), "got {err:?}");
        assert_eq!(current_branch(fx).as_deref(), Some("feature"));
        assert_eq!(fx.read(path), content);

        // `--clean` discards the work, so it skips the check entirely.
        oak_cli::commands::switch::run_with_policy(
            fx.dir(),
            Some("other"),
            false,
            WorktreePolicy::Discard,
        )
        .unwrap();
        assert_eq!(current_branch(fx).as_deref(), Some("other"));
        assert_eq!(fx.read("d3/f1.txt"), "d3f1\n");
        assert!(!fx.dir().join("scratch.txt").exists());
    });
}

/// `oak switch -c` probes the same yes/no: a clean tree is moved to `main`,
/// even when HEAD's own data is missing locally.
#[test]
fn switch_create_materializes_main_over_a_clean_tree_without_reading_head() {
    let fx = Fixture::new();
    fx.forget_legacy();

    oak_cli::commands::switch::create(fx.dir(), "fresh", WorktreePolicy::Carry).unwrap();

    let repo = fx.repo();
    assert_eq!(current_branch(&fx).as_deref(), Some("fresh"));
    assert_eq!(
        repo.get_branch_head("fresh").unwrap(),
        Some(fx.base.clone())
    );
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.dir().join("legacy").exists());
}

/// …and a dirty tree is carried onto the new branch as it is, not
/// overwritten with `main`.
#[test]
fn switch_create_carries_untracked_and_edited_files() {
    Fixture::each_dirty(|fx, path, content| {
        oak_cli::commands::switch::create(fx.dir(), "fresh", WorktreePolicy::Carry).unwrap();
        assert_eq!(current_branch(fx).as_deref(), Some("fresh"));
        assert_eq!(fx.read(path), content);
        assert_eq!(fx.read("d1/f0.txt"), "branch\n", "nothing was materialized");
        assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
    });
}

/// Splitting another branch never touches the working tree, so HEAD's
/// missing data can't matter; the old status guard refused it anyway.
#[test]
fn split_of_another_branch_runs_without_reading_head() {
    let fx = Fixture::new();
    let [c1, c2] = fx.topic();
    fx.forget_legacy();
    let plans = TempDir::new().unwrap();
    let plan = plan_file(&plans, &format!("pick {c1}\nsplit part2\npick {c2}\n"));

    oak_cli::commands::split::run(fx.dir(), Some("topic"), Some(&plan), false).unwrap();

    let repo = fx.repo();
    let topic = repo.get_branch_head("topic").unwrap().unwrap();
    assert_eq!(
        topic, c1,
        "the first pick replays onto the same base unchanged"
    );
    let part2 = repo.get_branch_head("part2").unwrap().unwrap();
    let part2 = repo.get_commit(&part2).unwrap().unwrap();
    assert_eq!(part2.parent_hash.as_ref(), Some(&fx.base));
    assert_eq!(repo.get_head().unwrap(), Some(fx.feature[1].clone()));
    assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
}

/// Splitting the checked-out branch moves the working tree to the new tip.
/// Dropping the commit that added `legacy` leaves a tip that doesn't need
/// HEAD's missing data, so the split runs.
#[test]
fn split_in_place_runs_without_reading_head() {
    let fx = Fixture::new();
    let [first, second] = fx.feature.clone();
    fx.forget_legacy();
    let plans = TempDir::new().unwrap();
    let plan = plan_file(&plans, &format!("pick {first}\ndrop {second}\n"));

    oak_cli::commands::split::run(fx.dir(), None, Some(&plan), false).unwrap();

    let repo = fx.repo();
    let tip = repo.get_branch_head("feature").unwrap().unwrap();
    assert_eq!(tip, first);
    assert_eq!(repo.get_head().unwrap(), Some(first));
    assert_eq!(fx.read("d1/f0.txt"), "branch\n");
    assert!(!fx.dir().join("legacy").exists());
}

#[test]
fn split_guard_refuses_untracked_and_edited_files() {
    Fixture::each_dirty(|fx, path, content| {
        let [first, second] = fx.feature.clone();
        let plans = TempDir::new().unwrap();
        let plan = plan_file(
            &plans,
            &format!("pick {first}\nsplit part2\npick {second}\n"),
        );
        let err = oak_cli::commands::split::run(fx.dir(), None, Some(&plan), false)
            .expect_err("a dirty tree refuses the split");
        assert!(matches!(err, OakError::UncommittedChanges), "got {err:?}");
        let repo = fx.repo();
        assert_eq!(repo.get_branch_head("feature").unwrap(), Some(second));
        assert!(repo.get_branch("part2").unwrap().is_none());
        assert_eq!(fx.read(path), content);
    });
}

/// The clean check no longer fails on a missing HEAD blob, and split moves
/// refs before the working tree. So it checks the blobs the move to the new
/// tip writes first: a missing one stops it before any ref, row or file
/// moves. Here the tip keeps only the second commit, so the move reverts
/// `d1/f0.txt` to `main`'s version, whose blob is gone.
#[test]
fn split_in_place_checks_the_new_tips_blobs_before_moving_refs() {
    let fx = Fixture::new();
    let [first, second] = fx.feature.clone();
    fx.forget_reverted_blob();
    let plans = TempDir::new().unwrap();
    let plan = plan_file(
        &plans,
        &format!("pick {second}\nsplit part2\npick {first}\n"),
    );

    let err = oak_cli::commands::split::run(fx.dir(), None, Some(&plan), false)
        .expect_err("a missing blob stops the split");
    assert!(
        matches!(err, OakError::IncompleteBlobData { .. }),
        "got {err:?}"
    );

    let repo = fx.repo();
    assert_eq!(
        repo.get_branch_head("feature").unwrap(),
        Some(second.clone())
    );
    assert_eq!(repo.get_head().unwrap(), Some(second));
    assert!(repo.get_branch("part2").unwrap().is_none());
    assert_eq!(fx.read("d1/f0.txt"), "branch\n");
    assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
}

#[test]
fn branch_from_guard_proves_clean_without_reading_head() {
    let fx = Fixture::new();
    fx.forget_legacy();
    let base = fx.base.to_string();

    oak_cli::commands::branch::new_branch(fx.dir(), "pinned", None, Some(&base), None).unwrap();

    let repo = fx.repo();
    assert_eq!(current_branch(&fx).as_deref(), Some("pinned"));
    assert_eq!(
        repo.get_branch_head("pinned").unwrap(),
        Some(fx.base.clone())
    );
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.dir().join("legacy").exists());
}

#[test]
fn branch_from_guard_refuses_untracked_and_edited_files() {
    Fixture::each_dirty(|fx, path, content| {
        let base = fx.base.to_string();
        let err =
            oak_cli::commands::branch::new_branch(fx.dir(), "pinned", None, Some(&base), None)
                .expect_err("a dirty tree refuses the seeded branch");
        assert!(matches!(err, OakError::UncommittedChanges), "got {err:?}");
        assert!(fx.repo().get_branch("pinned").unwrap().is_none());
        assert_eq!(current_branch(fx).as_deref(), Some("feature"));
        assert_eq!(fx.read(path), content);
    });
}

/// `oak branch --from` stores the branch row before moving the working
/// tree, so it checks the blobs the move to the seed writes first: a missing
/// one leaves no half-created branch behind.
#[test]
fn branch_from_checks_the_seeds_blobs_before_storing_the_branch() {
    let fx = Fixture::new();
    fx.forget_reverted_blob();
    let base = fx.base.to_string();

    let err = oak_cli::commands::branch::new_branch(fx.dir(), "pinned", None, Some(&base), None)
        .expect_err("a missing blob stops the seeded branch");
    assert!(
        matches!(err, OakError::IncompleteBlobData { .. }),
        "got {err:?}"
    );
    assert!(fx.repo().get_branch("pinned").unwrap().is_none());
    assert_eq!(current_branch(&fx).as_deref(), Some("feature"));
    assert_eq!(fx.read("d1/f0.txt"), "branch\n");
    assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
}
