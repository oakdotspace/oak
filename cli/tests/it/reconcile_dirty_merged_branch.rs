//! A branch squash-merged on the server while it sat behind `main`, with
//! uncommitted work on disk. `oak fetch`, `oak status --reconcile` and
//! `oak pull` all learn of the merge and retire the branch; none may touch the
//! files, and the branch they land on must keep status down to the real edits.
//! Basing it on main's head would show everything main gained since the fork
//! as uncommitted, and the next `oak commit` would revert it.
use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, Commit, Hash, MetadataKey, Repository, SqliteRepository};
use serde_json::Value;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn open(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn commit_tree(dir: &Path) -> Hash {
    assert_ok(&oak(dir, &["commit"]));
    let repo = open(dir);
    let head = repo.get_head().unwrap().unwrap();
    repo.get_commit(&head).unwrap().unwrap().manifest_hash
}

fn info(commit: &Commit) -> Value {
    serde_json::json!({
        "hash": commit.hash.to_string(),
        "branch_name": commit.branch_name.clone(),
        "parent_hash": commit.parent_hash.as_ref().map(|h| h.to_string()),
        "merge_parent_hash": commit.merge_parent_hash.as_ref().map(|h| h.to_string()),
        "manifest_hash": commit.manifest_hash.to_string(),
        "author": commit.author.clone(),
        "message": commit.message.clone(),
        "timestamp": commit.timestamp.to_rfc3339(),
        "files": []
    })
}

fn main_commit(root: &Hash, parent: &Hash, merge_parent: Option<&Hash>, n: i64) -> Commit {
    Commit::with_timestamp(
        "main".into(),
        Some(parent.clone()),
        merge_parent.cloned(),
        root.clone(),
        "<remote>".into(),
        Some(format!("main {n}")),
        Vec::new(),
        chrono::DateTime::from_timestamp(1_700_000_000 + n, 0).unwrap(),
    )
    .unwrap()
}

struct Fx {
    temp: tempfile::TempDir,
    _server: MockServer,
    /// The merged tip of `work`, forked three commits behind the squash.
    tip: Hash,
    /// Main's head on the server: fork + 3 commits + squash + 1 commit.
    main_head: Hash,
    /// Main's commits above the fork, oldest first.
    main_commits: Vec<Commit>,
}

impl Fx {
    fn dir(&self) -> &Path {
        self.temp.path()
    }

    /// Make the local `main` ref hold what `oak fetch` would have brought in,
    /// without running the reconcile `oak fetch` performs.
    fn refresh_main_only(&self) {
        let repo = open(self.dir());
        for commit in &self.main_commits {
            repo.store_commit(commit).unwrap();
        }
        repo.set_branch_head("main", &self.main_head).unwrap();
    }
}

/// Local history: main at `fork`, `work` one commit past it. Remote main
/// gained three commits, then the squash of `work` (merge parent = the tip),
/// then one more. Every tree and blob is already local, so the mock only
/// serves commit metadata.
async fn fixture() -> Fx {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert_ok(&oak(dir, &["init", "."]));

    // Build the trees main will hold as real commits on a scaffold branch, so
    // their trees and blobs land in the local store.
    std::fs::write(dir.join("shared.txt"), "fork\n").unwrap();
    let fork_root = commit_tree(dir);
    std::fs::write(dir.join("later-1.txt"), "one\n").unwrap();
    let root_1 = commit_tree(dir);
    std::fs::write(dir.join("later-2.txt"), "two\n").unwrap();
    let root_2 = commit_tree(dir);
    std::fs::write(dir.join("shared.txt"), "main moved this\n").unwrap();
    let root_3 = commit_tree(dir);
    std::fs::write(dir.join("work.txt"), "work\n").unwrap();
    let squash_root = commit_tree(dir);
    std::fs::write(dir.join("later-3.txt"), "three\n").unwrap();
    let head_root = commit_tree(dir);

    // Put the tree back to the fork, plus the branch's own file, and record
    // that as `work`'s one commit on top of main's fork commit.
    for name in ["later-1.txt", "later-2.txt", "later-3.txt"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    std::fs::write(dir.join("shared.txt"), "fork\n").unwrap();
    let repo = open(dir);
    let fork = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        fork_root,
        "<remote>".into(),
        Some("fork".into()),
        Vec::new(),
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    )
    .unwrap();
    repo.store_commit(&fork).unwrap();
    if repo.get_branch("main").unwrap().is_none() {
        repo.store_branch(&Branch::new("main".into(), None, None))
            .unwrap();
    }
    repo.set_branch_head("main", &fork.hash).unwrap();
    repo.store_branch(&Branch::new("work".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("work", &fork.hash).unwrap();
    repo.set_current_branch("work").unwrap();
    repo.set_head(&fork.hash).unwrap();
    drop(repo);
    assert_ok(&oak(dir, &["commit"]));

    let repo = open(dir);
    let tip = repo.get_branch_head("work").unwrap().unwrap();
    let m1 = main_commit(&root_1, &fork.hash, None, 1);
    let m2 = main_commit(&root_2, &m1.hash, None, 2);
    let m3 = main_commit(&root_3, &m2.hash, None, 3);
    let squash = main_commit(&squash_root, &m3.hash, Some(&tip), 4);
    let head = main_commit(&head_root, &squash.hash, None, 5);
    let main_commits = vec![m1, m2, m3, squash, head];
    let main_head = main_commits.last().unwrap().hash.clone();

    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();

    let mut read_tree = |h: &Hash| -> oak_core::Result<oak_core::Tree> {
        repo.get_tree(h)?
            .ok_or_else(|| oak_core::OakError::ManifestNotFound(h.to_string()))
    };
    let trees: Vec<Value> = oak_core::collect_tree_objects(&head_root, &mut read_tree)
        .unwrap()
        .iter()
        .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
        .collect();
    let tip_commit = repo.get_commit(&tip).unwrap().unwrap();
    let entries: Vec<Value> = std::iter::once(&fork)
        .chain(std::iter::once(&tip_commit))
        .chain(main_commits.iter())
        .map(info)
        .collect();
    drop(repo);

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "head": main_head.to_string() })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "commits": entries, "trees": trees })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/work"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    // The server closed `work` when it merged it, and has nothing else new.
    let pull_response = |branch: Value| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": branch,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        }))
    };
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .and(query_param("branch", "work"))
        .respond_with(pull_response(serde_json::json!({
            "name": "work",
            "description": null,
            "parent_branch": "main",
            "status": "closed",
            "created_at": chrono::Utc::now().to_rfc3339(),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(pull_response(Value::Null))
        .mount(&server)
        .await;

    Fx {
        temp,
        _server: server,
        tip,
        main_head,
        main_commits,
    }
}

fn status_paths(dir: &Path) -> Vec<String> {
    let out = oak(dir, &["status", "--json"]);
    assert_ok(&out);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut paths: Vec<String> = json["working_changes"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths
}

fn disk(dir: &Path) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

/// The state every path must land in: rotated off the merged branch, onto a
/// branch at the merged tip, with main ahead and the disk untouched.
fn assert_rotated_onto_tip(fx: &Fx, before: &[(String, String)], expected: &[&str]) -> String {
    let dir = fx.dir();
    assert_eq!(status_paths(dir), expected);
    assert_eq!(disk(dir), before, "reconcile must not touch any file");

    let repo = open(dir);
    let current = repo.get_current_branch_name().unwrap().unwrap();
    assert_ne!(current, "work", "the merged branch must be retired");
    assert_eq!(
        repo.get_branch(&current)
            .unwrap()
            .unwrap()
            .parent_branch
            .as_deref(),
        Some("main")
    );
    assert_eq!(
        repo.get_branch_head(&current).unwrap(),
        Some(fx.tip.clone())
    );
    assert_eq!(repo.get_head().unwrap(), Some(fx.tip.clone()));
    assert_eq!(
        repo.get_branch_head("main").unwrap(),
        Some(fx.main_head.clone())
    );
    current
}

#[tokio::test]
async fn fetch_keeps_status_to_the_real_edit_on_a_branch_behind_main() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("note.txt"), "uncommitted\n").unwrap();
    let before = disk(dir);

    assert_ok(&oak(dir, &["fetch"]));
    let rotated = assert_rotated_onto_tip(&fx, &before, &["note.txt"]);

    // The seed is itself a merged tip: another fetch must not rotate again.
    assert_ok(&oak(dir, &["fetch"]));
    assert_eq!(
        assert_rotated_onto_tip(&fx, &before, &["note.txt"]),
        rotated
    );
}

#[tokio::test]
async fn fetch_keeps_an_edit_to_a_file_main_also_changed() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("shared.txt"), "my uncommitted edit\n").unwrap();
    let before = disk(dir);

    assert_ok(&oak(dir, &["fetch"]));
    assert_rotated_onto_tip(&fx, &before, &["shared.txt"]);
}

#[tokio::test]
async fn status_reconcile_keeps_status_to_the_real_edit() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("note.txt"), "uncommitted\n").unwrap();
    fx.refresh_main_only();
    let before = disk(dir);

    assert_ok(&oak(dir, &["status", "--reconcile"]));
    let rotated = assert_rotated_onto_tip(&fx, &before, &["note.txt"]);

    assert_ok(&oak(dir, &["status", "--reconcile"]));
    assert_eq!(
        assert_rotated_onto_tip(&fx, &before, &["note.txt"]),
        rotated
    );
}

/// Pull runs the same reconcile from its parent sync, which would otherwise
/// write main's changes straight over the edit.
#[tokio::test]
async fn pull_keeps_the_edit_and_reports_main_as_not_synced() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("shared.txt"), "my uncommitted edit\n").unwrap();
    let before = disk(dir);

    let out = oak(dir, &["pull", "--json"]);
    assert_ok(&out);
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["parent_sync"], "reconciled_merged_branch");
    assert_eq!(receipt["parent_synced"], false);
    assert_rotated_onto_tip(&fx, &before, &["shared.txt"]);
}

/// The way forward the warning names: commit, then pull in main. Only the
/// edit is new; main's history must survive.
#[tokio::test]
async fn commit_then_pull_brings_main_in_without_reverting_it() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("note.txt"), "uncommitted\n").unwrap();
    assert_ok(&oak(dir, &["fetch"]));

    assert_ok(&oak(dir, &["commit"]));
    let out = oak(dir, &["pull"]);
    assert_ok(&out);

    assert_eq!(status_paths(dir), Vec::<String>::new());
    assert_eq!(
        disk(dir),
        [
            ("later-1.txt", "one\n"),
            ("later-2.txt", "two\n"),
            ("later-3.txt", "three\n"),
            ("note.txt", "uncommitted\n"),
            ("shared.txt", "main moved this\n"),
            ("work.txt", "work\n"),
        ]
        .map(|(path, content)| (path.to_string(), content.to_string()))
    );
    let repo = open(dir);
    let head = repo.get_head().unwrap().unwrap();
    assert_eq!(
        repo.get_commit(&head).unwrap().unwrap().merge_parent_hash,
        Some(fx.main_head.clone())
    );
}

/// Discarding the edit leaves nothing to keep the branch back: it moves on to
/// main like any clean reconcile.
#[tokio::test]
async fn discarding_the_edit_lets_the_next_reconcile_reach_main() {
    let fx = fixture().await;
    let dir = fx.dir();
    std::fs::write(dir.join("note.txt"), "uncommitted\n").unwrap();
    assert_ok(&oak(dir, &["fetch"]));

    assert_ok(&oak(dir, &["reset", "-f"]));
    assert_ok(&oak(dir, &["status", "--reconcile"]));

    assert_eq!(status_paths(dir), Vec::<String>::new());
    let repo = open(dir);
    assert_eq!(repo.get_head().unwrap(), Some(fx.main_head.clone()));
    assert_eq!(
        std::fs::read_to_string(dir.join("later-3.txt")).unwrap(),
        "three\n"
    );
}

/// Control: a clean tree still lands on main.
#[tokio::test]
async fn fetch_on_a_clean_tree_still_moves_to_main() {
    let fx = fixture().await;
    let dir = fx.dir();

    assert_ok(&oak(dir, &["fetch"]));

    assert_eq!(status_paths(dir), Vec::<String>::new());
    let repo = open(dir);
    assert_eq!(repo.get_head().unwrap(), Some(fx.main_head.clone()));
    assert_eq!(
        std::fs::read_to_string(dir.join("shared.txt")).unwrap(),
        "main moved this\n"
    );
}
