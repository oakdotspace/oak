//! `oak switch`, `oak switch -c`, in-place `oak split` and `oak branch
//! --from` move the working tree by tree diff once their dirty guard has
//! proved it holds HEAD's snapshot, the way `oak merge` and `oak pull`'s
//! fast-forward do. These tests pin what that buys and what it keeps:
//!
//! - each command reads only the directories the two snapshots disagree on,
//!   so it runs when the trees and blobs of directories both share are
//!   missing locally (the whole-tree rewrite, which `--clean` still does,
//!   needs every one);
//! - what the dirty guard doesn't see — an empty directory, a socket — is
//!   left alone (the whole-tree rewrite swept it away);
//! - files whose content was withheld (restricted) or declared lost are
//!   skipped with a warning, as the whole-tree rewrite skips them;
//! - `oak switch` fetches from the remote only the blobs the move writes;
//!   for a branch only the remote has, the pull just fetches, and the same
//!   move writes the tree (the pull used to write every file first).

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixListener;
use std::path::Path;

use oak_cli::commands::switch::WorktreePolicy;
use oak_core::{Commit, FileMode, Hash, ManifestEntry, MetadataKey};
use oak_core::{Repository, SqliteRepository};
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn init_repo(dir: &Path) -> SqliteRepository {
    std::env::set_var("OAK_AUTHOR", "tester");
    let oak_dir = dir.join(".oak");
    fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "testowner")
        .unwrap();
    repo.set_metadata(MetadataKey::RepoName, "demo").unwrap();
    // No remote unless a test adds one: `main` is resolved from its local
    // head, so every flow stays offline.
    repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
        .unwrap();
    repo.store_branch(&oak_core::Branch::new(
        "feature".to_string(),
        None,
        Some("main".to_string()),
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
    let mut out = without(entries, path);
    out.push(entry(path, blob));
    out
}

fn without(entries: &[ManifestEntry], path: &str) -> Vec<ManifestEntry> {
    entries.iter().filter(|e| e.path != path).cloned().collect()
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

fn new_branch(repo: &SqliteRepository, name: &str) {
    repo.store_branch(&oak_core::Branch::new(
        name.to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
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

/// `main` holds eight directories of four files each. `feature`, checked
/// out, edits `d1/f0.txt` in one commit and adds `legacy/old.txt` in a
/// second.
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

    fn exists(&self, path: &str) -> bool {
        fs::symlink_metadata(self.dir().join(path)).is_ok()
    }

    fn current_branch(&self) -> Option<String> {
        self.repo().get_current_branch_name().unwrap()
    }

    /// `other`: one commit on `main` that edits `d2/f0.txt`, adds
    /// `d2/new.txt` and deletes `d3/f1.txt`.
    fn other(&self) -> Hash {
        let repo = self.repo();
        new_branch(&repo, "other");
        let entries = without(
            &with(
                &with(&self.base_entries, "d2/f0.txt", &blob(&repo, "other\n")),
                "d2/new.txt",
                &blob(&repo, "new\n"),
            ),
            "d3/f1.txt",
        );
        commit(&repo, "other", Some(&self.base), entries)
    }

    /// The tree of `d5`, which every snapshot here shares.
    fn d5_tree(&self) -> Hash {
        let repo = self.repo();
        let root = repo.get_commit(&self.base).unwrap().unwrap().manifest_hash;
        repo.subtree_at_path(&root, "d5").unwrap().unwrap()
    }

    /// Drop `d5`'s tree and the blobs of `d5` and `d6` — directories no
    /// command here changes — from the local store, leaving the files on
    /// disk. Flattening any snapshot now fails.
    fn forget_untouched(&self) -> Hash {
        let d5 = self.d5_tree();
        let blobs: Vec<Hash> = self
            .base_entries
            .iter()
            .filter(|e| e.path.starts_with("d5/") || e.path.starts_with("d6/"))
            .map(|e| e.blob_hash.clone())
            .collect();
        delete_rows(self.dir(), "trees", std::slice::from_ref(&d5));
        delete_rows(self.dir(), "blobs", &blobs);
        let repo = self.repo();
        let root = repo.get_commit(&self.base).unwrap().unwrap().manifest_hash;
        assert!(repo.get_manifest(&root).is_err(), "{root} can't flatten");
        d5
    }

    /// Untracked content the dirty guard doesn't see, so it never refuses
    /// over it: an empty directory and a socket (say, a dev server's). The
    /// whole-tree rewrite deleted both.
    fn add_unseen_extras(&self) -> UnixListener {
        fs::create_dir(self.dir().join("scratch")).unwrap();
        UnixListener::bind(self.dir().join("dev.sock")).unwrap()
    }

    fn assert_unseen_extras_survive(&self) {
        assert!(self.dir().join("scratch").is_dir(), "empty dir kept");
        let socket = fs::symlink_metadata(self.dir().join("dev.sock")).unwrap();
        assert!(socket.file_type().is_socket(), "socket kept");
    }

    /// What every move here leaves alone: the untouched directories' files
    /// on disk, and their tree still missing from the store.
    fn assert_untouched(&self, d5: &Hash) {
        assert_eq!(self.read("d5/f0.txt"), "d5f0\n");
        assert_eq!(self.read("d6/f3.txt"), "d6f3\n");
        assert!(
            self.repo().get_tree(d5).unwrap().is_none(),
            "nothing read or fetched d5"
        );
    }
}

/// Write a split plan outside the working tree (inside it, the plan file
/// would itself be untracked work).
fn plan_file(plans: &TempDir, text: &str) -> String {
    let path = plans.path().join("plan.txt");
    fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn switch_moves_by_diff_without_reading_untouched_directories() {
    let fx = Fixture::new();
    let other = fx.other();
    let d5 = fx.forget_untouched();
    let _socket = fx.add_unseen_extras();

    // `--clean` still rewrites the whole tree, which needs every tree.
    oak_cli::commands::switch::run_with_policy(
        fx.dir(),
        Some("other"),
        false,
        WorktreePolicy::Discard,
    )
    .expect_err("the whole-tree rewrite flattens `other`");
    assert_eq!(fx.current_branch().as_deref(), Some("feature"));
    assert!(fx.exists("legacy/old.txt"));

    oak_cli::commands::switch::run(fx.dir(), Some("other"), false).unwrap();
    assert_eq!(fx.current_branch().as_deref(), Some("other"));
    assert_eq!(fx.repo().get_head().unwrap(), Some(other));
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert_eq!(fx.read("d2/f0.txt"), "other\n");
    assert_eq!(fx.read("d2/new.txt"), "new\n");
    assert!(!fx.exists("d3/f1.txt"));
    assert!(!fx.exists("legacy"), "the emptied directory is pruned");
    fx.assert_untouched(&d5);
    fx.assert_unseen_extras_survive();

    // Detached at a commit, and back onto the branch.
    let base = fx.base.to_string();
    oak_cli::commands::switch::run(fx.dir(), Some(&base), true).unwrap();
    assert_eq!(fx.current_branch(), None);
    assert_eq!(fx.read("d2/f0.txt"), "d2f0\n");
    assert!(!fx.exists("d2/new.txt"));
    assert_eq!(fx.read("d3/f1.txt"), "d3f1\n");

    oak_cli::commands::switch::run(fx.dir(), Some("feature"), false).unwrap();
    assert_eq!(fx.current_branch().as_deref(), Some("feature"));
    assert_eq!(fx.read("d1/f0.txt"), "branch\n");
    assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
    fx.assert_untouched(&d5);
    fx.assert_unseen_extras_survive();

    // The stat cache rows of the files the moves wrote are current, so the
    // tree reads as clean without re-hashing them.
    let repo = fx.repo();
    let cache = repo.load_stat_cache().unwrap();
    assert!(!cache.contains_key("d2/new.txt"));
    assert_eq!(
        cache["legacy/old.txt"].blob_hash,
        oak_core::hash_bytes(b"legacy\n")
    );
    assert!(
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, fx.dir())
            .unwrap()
    );
}

/// `--clean` discards uncommitted work, so it still rewrites the whole tree
/// and sweeps away what the guard doesn't see.
#[test]
fn switch_clean_still_rewrites_the_whole_tree() {
    let fx = Fixture::new();
    fx.other();
    let _socket = fx.add_unseen_extras();

    oak_cli::commands::switch::run_with_policy(
        fx.dir(),
        Some("other"),
        false,
        WorktreePolicy::Discard,
    )
    .unwrap();
    assert_eq!(fx.read("d2/f0.txt"), "other\n");
    assert!(!fx.exists("scratch"));
    assert!(!fx.exists("dev.sock"));
}

/// `oak switch -c` materializes `main` over a clean tree by the diff from
/// HEAD.
#[test]
fn switch_create_moves_by_diff_without_reading_untouched_directories() {
    let fx = Fixture::new();
    let d5 = fx.forget_untouched();
    let _socket = fx.add_unseen_extras();

    oak_cli::commands::switch::create(fx.dir(), "fresh", WorktreePolicy::Carry).unwrap();

    assert_eq!(fx.current_branch().as_deref(), Some("fresh"));
    assert_eq!(
        fx.repo().get_branch_head("fresh").unwrap(),
        Some(fx.base.clone())
    );
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.exists("legacy"));
    fx.assert_untouched(&d5);
    fx.assert_unseen_extras_survive();
}

/// In-place `oak split` moves the tree to the rewritten tip by the diff from
/// the old one. Dropping the first commit reverts `d1/f0.txt` and keeps
/// `legacy/old.txt`.
#[test]
fn split_in_place_moves_by_diff_without_reading_untouched_directories() {
    let fx = Fixture::new();
    let [first, second] = fx.feature.clone();
    let d5 = fx.forget_untouched();
    let _socket = fx.add_unseen_extras();
    let plans = TempDir::new().unwrap();
    let plan = plan_file(&plans, &format!("drop {first}\npick {second}\n"));

    oak_cli::commands::split::run(fx.dir(), None, Some(&plan), false).unwrap();

    let repo = fx.repo();
    let tip = repo.get_branch_head("feature").unwrap().unwrap();
    assert_ne!(tip, second);
    assert_eq!(repo.get_head().unwrap(), Some(tip.clone()));
    let tip = repo.get_commit(&tip).unwrap().unwrap();
    assert_eq!(tip.parent_hash.as_ref(), Some(&fx.base));
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert_eq!(fx.read("legacy/old.txt"), "legacy\n");
    fx.assert_untouched(&d5);
    fx.assert_unseen_extras_survive();
}

#[test]
fn branch_from_moves_by_diff_without_reading_untouched_directories() {
    let fx = Fixture::new();
    let d5 = fx.forget_untouched();
    let _socket = fx.add_unseen_extras();
    let base = fx.base.to_string();

    oak_cli::commands::branch::new_branch(fx.dir(), "pinned", None, Some(&base), None).unwrap();

    assert_eq!(fx.current_branch().as_deref(), Some("pinned"));
    assert_eq!(fx.repo().get_head().unwrap(), Some(fx.base.clone()));
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.exists("legacy"));
    fx.assert_untouched(&d5);
    fx.assert_unseen_extras_survive();
}

/// `withheld`: one commit on `main` editing `d2/f0.txt` to content the
/// server withheld (restricted), `d4/f0.txt` to content declared lost, and
/// `d2/f1.txt` to ordinary content, and adding `hidden/secret.txt` with
/// restricted content. Neither withheld blob is stored.
fn withheld(fx: &Fixture) -> Hash {
    let repo = fx.repo();
    new_branch(&repo, "withheld");
    let restricted = oak_core::hash_bytes(b"restricted\n");
    let lost = oak_core::hash_bytes(b"lost\n");
    oak_cli::commands::restricted::record_restricted_blobs(&repo, &[restricted.to_string()])
        .unwrap();
    oak_cli::commands::known_loss::record_known_lost_blobs(&repo, &[lost.to_string()]).unwrap();
    let entries = with(
        &with(
            &with(&fx.base_entries, "d2/f0.txt", &restricted),
            "d4/f0.txt",
            &lost,
        ),
        "d2/f1.txt",
        &blob(&repo, "ordinary\n"),
    );
    let entries = with(&entries, "hidden/secret.txt", &restricted);
    commit(&repo, "withheld", Some(&fx.base), entries)
}

/// The diff skips withheld files exactly as the whole-tree rewrite does: a
/// restricted file isn't written (what was there stays, and a new one leaves
/// no empty directory), a lost one is removed, and the switch goes through.
#[test]
fn switch_skips_withheld_files_like_the_whole_tree_rewrite() {
    for policy in [WorktreePolicy::Carry, WorktreePolicy::Discard] {
        let fx = Fixture::new();
        let head = withheld(&fx);

        oak_cli::commands::switch::run_with_policy(fx.dir(), Some("withheld"), false, policy)
            .unwrap();

        assert_eq!(fx.repo().get_head().unwrap(), Some(head), "{policy:?}");
        assert_eq!(fx.read("d2/f1.txt"), "ordinary\n", "{policy:?}");
        assert_eq!(fx.read("d2/f0.txt"), "d2f0\n", "{policy:?}");
        assert!(!fx.exists("d4/f0.txt"), "{policy:?}");
        assert!(!fx.exists("hidden"), "{policy:?}");
        assert_eq!(fx.read("d1/f0.txt"), "d1f0\n", "{policy:?}");
    }
}

/// `oak branch --from` checks blobs before storing the branch; withheld
/// files don't count as missing there either.
#[test]
fn branch_from_skips_withheld_files() {
    let fx = Fixture::new();
    let head = withheld(&fx);

    oak_cli::commands::branch::new_branch(fx.dir(), "pinned", None, Some(head.as_str()), None)
        .unwrap();

    assert_eq!(fx.repo().get_head().unwrap(), Some(head));
    assert_eq!(fx.read("d2/f1.txt"), "ordinary\n");
    assert_eq!(fx.read("d2/f0.txt"), "d2f0\n");
    assert!(!fx.exists("d4/f0.txt"));
    assert!(!fx.exists("hidden"));
}

/// `oak switch` hydrates from the remote the blobs the move writes and
/// nothing else: the untouched directories' missing blobs stay missing.
#[test]
fn switch_fetches_only_the_blobs_the_move_writes() {
    let fx = Fixture::new();
    fx.other();
    let d5 = fx.forget_untouched();
    let content = b"other\n".to_vec();
    let wanted = oak_core::hash_bytes(&content);
    delete_rows(fx.dir(), "blobs", std::slice::from_ref(&wanted));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        let exactly_wanted = {
            let wanted = wanted.to_string();
            move |request: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                body["hashes"] == json!([wanted])
            }
        };
        Mock::given(method("POST"))
            .and(path("/api/testowner/demo/blobs/info"))
            .and(exactly_wanted.clone())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "blobs": [{
                    "hash": wanted.as_str(),
                    "size": content.len(),
                    "chunks": [{ "hash": wanted.as_str(), "offset": 0, "size": content.len() }],
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/testowner/demo/chunks/download"))
            .and(exactly_wanted)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunks": [{ "hash": wanted.as_str(), "content": content }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        server
    });
    fx.repo()
        .set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();

    oak_cli::commands::switch::run(fx.dir(), Some("other"), false).unwrap();

    assert_eq!(fx.read("d2/f0.txt"), "other\n");
    fx.assert_untouched(&d5);
    rt.block_on(server.verify());
}

/// A branch only the remote has: one commit on `main` that edits `d2/f0.txt`
/// to `"<name>\n"`, adds `d2/new.txt` and deletes `d3/f1.txt`. Its trees
/// are stored locally, like the ones it shares with local snapshots (the
/// response ships none); its commit, its row and the edited file's blob
/// (returned, for the response to ship) are not.
fn remote_branch(fx: &Fixture, name: &str) -> (Commit, Vec<u8>) {
    let repo = fx.repo();
    let content = format!("{name}\n").into_bytes();
    let entries = without(
        &with(
            &with(
                &fx.base_entries,
                "d2/f0.txt",
                &oak_core::hash_bytes(&content),
            ),
            "d2/new.txt",
            &blob(&repo, "new\n"),
        ),
        "d3/f1.txt",
    );
    let head = Commit::with_timestamp(
        name.to_string(),
        Some(fx.base.clone()),
        None,
        repo.put_manifest(entries).unwrap(),
        "peer".to_string(),
        None,
        Vec::new(),
        chrono::DateTime::from_timestamp(1_700_000_500, 0).unwrap(),
    )
    .unwrap();
    (head, content)
}

/// Answer one pull of branch `name` (`status` "open" or "closed") with its
/// one commit `head`, shipping `content`'s blob through the chunk endpoint.
fn serve_remote_branch(
    rt: &tokio::runtime::Runtime,
    fx: &Fixture,
    name: &str,
    status: &str,
    head: &Commit,
    content: &[u8],
) -> MockServer {
    let blob = oak_core::hash_bytes(content);
    let branch = json!({
        "name": name,
        "description": null,
        "parent_branch": "main",
        "status": status,
        "created_at": "2026-01-01T00:00:00Z",
    });
    let pull = json!({
        "head": head.hash.as_str(),
        "branch": branch,
        "branches": [branch],
        "commits": [{
            "hash": head.hash.as_str(),
            "branch_name": name,
            "parent_hash": fx.base.as_str(),
            "manifest_hash": head.manifest_hash.as_str(),
            "author": head.author,
            "timestamp": head.timestamp.to_rfc3339(),
            "files": [],
        }],
        "blobs": [{
            "hash": blob.as_str(),
            "content": [],
            "size": content.len(),
            "chunks": [{ "hash": blob.as_str(), "offset": 0, "size": content.len() }],
        }],
        "trees": [],
    });
    let chunks = json!({ "chunks": [{ "hash": blob.as_str(), "content": content }] });
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/testowner/demo/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/testowner/demo/chunks/download"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chunks))
            .expect(1)
            .mount(&server)
            .await;
        server
    });
    fx.repo()
        .set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    server
}

/// `oak switch NAME` for a branch only the remote has: the pull only fetches,
/// and the switch's move writes what differs from HEAD's snapshot. The
/// untouched directories' tree and blobs are missing locally and never read
/// (the pull used to write every file of the fetched head first, which
/// needed them all), and a file both snapshots share is left as it was.
#[test]
fn switch_to_a_remote_branch_writes_only_what_differs_from_head() {
    let fx = Fixture::new();
    let (head, content) = remote_branch(&fx, "remote");
    let d5 = fx.forget_untouched();
    let shared = fs::metadata(fx.dir().join("d0/f0.txt")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = serve_remote_branch(&rt, &fx, "remote", "open", &head, &content);

    oak_cli::commands::switch::run(fx.dir(), Some("remote"), false).unwrap();

    let repo = fx.repo();
    assert_eq!(fx.current_branch().as_deref(), Some("remote"));
    assert_eq!(
        repo.get_branch_head("remote").unwrap(),
        Some(head.hash.clone())
    );
    assert_eq!(repo.get_head().unwrap(), Some(head.hash.clone()));
    assert_eq!(fx.read("d2/f0.txt"), "remote\n");
    assert_eq!(fx.read("d2/new.txt"), "new\n");
    assert!(!fx.exists("d3/f1.txt"));
    assert_eq!(fx.read("d1/f0.txt"), "d1f0\n");
    assert!(!fx.exists("legacy"), "the emptied directory is pruned");
    fx.assert_untouched(&d5);
    let after = fs::metadata(fx.dir().join("d0/f0.txt")).unwrap();
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (shared.ino(), shared.mtime(), shared.mtime_nsec()),
        "a file both snapshots share isn't rewritten"
    );
    assert!(
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, fx.dir())
            .unwrap()
    );
    rt.block_on(server.verify());
}

/// A branch the remote already merged is refused before anything moves. The
/// pull used to write its snapshot into the working tree and point HEAD at
/// it first, leaving both there after the refusal.
#[test]
fn switch_to_a_branch_merged_on_the_remote_leaves_the_checkout_alone() {
    let fx = Fixture::new();
    let (head, content) = remote_branch(&fx, "merged");
    let before = fx.repo().get_head().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = serve_remote_branch(&rt, &fx, "merged", "closed", &head, &content);

    let err = oak_cli::commands::switch::run(fx.dir(), Some("merged"), false).unwrap_err();
    assert!(err.to_string().contains("already merged"), "got: {err}");

    let repo = fx.repo();
    assert!(repo.get_branch("merged").unwrap().is_none());
    assert_eq!(fx.current_branch().as_deref(), Some("feature"));
    assert_eq!(repo.get_head().unwrap(), before);
    assert_eq!(fx.read("d2/f0.txt"), "d2f0\n");
    assert!(!fx.exists("d2/new.txt"));
    assert_eq!(fx.read("d3/f1.txt"), "d3f1\n");
    assert!(
        oak_cli::commands::commit::worktree_is_clean_without_storing_blobs(&repo, fx.dir())
            .unwrap()
    );
    rt.block_on(server.verify());
}

/// `oak switch NAME --clean` over uncommitted work, for a branch only the
/// remote has, discards the work with the switch's whole-tree rewrite. The
/// pull's own dirty guard used to refuse first, and the switch then reported
/// the branch missing.
#[test]
fn switch_clean_to_a_remote_branch_discards_uncommitted_work() {
    let fx = Fixture::new();
    let (head, content) = remote_branch(&fx, "remote");
    fs::write(fx.dir().join("d0/f0.txt"), "uncommitted\n").unwrap();
    fs::write(fx.dir().join("notes.txt"), "untracked\n").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = serve_remote_branch(&rt, &fx, "remote", "open", &head, &content);

    oak_cli::commands::switch::run_with_policy(
        fx.dir(),
        Some("remote"),
        false,
        WorktreePolicy::Discard,
    )
    .unwrap();

    assert_eq!(fx.current_branch().as_deref(), Some("remote"));
    assert_eq!(fx.repo().get_head().unwrap(), Some(head.hash.clone()));
    assert_eq!(fx.read("d0/f0.txt"), "d0f0\n");
    assert!(!fx.exists("notes.txt"));
    assert_eq!(fx.read("d2/f0.txt"), "remote\n");
    assert!(!fx.exists("legacy"));
    rt.block_on(server.verify());
}
