//! Regression (from independent QA review QA-1 of mrmrs-7920de): after a
//! head-only parent refresh *moves* local `main` past a squash merge of another
//! branch, local merge-base readers (`oak diff <branch>`, status
//! `branch_changes`, merge previews) must stay as answerable as with 0.103.0's
//! complete refresh. A moved head therefore always links the complete
//! ancestry; only an already-local, hydrated head skips the ancestry walk.
//! `QA1_OAK_BIN` replays the tests against another binary (e.g. a release).
use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, Commit, Hash, MetadataKey, Repository, SqliteRepository};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(
        std::env::var("QA1_OAK_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_oak").to_string()),
    )
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

struct Fx {
    temp: tempfile::TempDir,
    _server: MockServer,
    m0: Hash,
    x_tip: Hash,
    m2: Hash,
}

/// Local: complete history. main = M0 (root), branch `work` forked at M0 with
/// one commit. Remote: M1 = squash of feature-x (merge parent X, X's parent
/// M0), M2 = head on top of M1. All share one manifest so no blob traffic.
async fn fixture() -> Fx {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert_ok(&oak(dir, &["init", "."]));
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert_ok(&oak(dir, &["commit"]));

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let first_branch = repo.get_current_branch_name().unwrap().unwrap();
    let local_head = repo.get_branch_head(&first_branch).unwrap().unwrap();
    let manifest = repo.get_commit(&local_head).unwrap().unwrap().manifest_hash;
    let at = |i: i64| chrono::DateTime::from_timestamp(1_700_000_000 + i, 0).unwrap();

    let m0 = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        manifest.clone(),
        "<remote>".into(),
        Some("m0".into()),
        Vec::new(),
        at(0),
    )
    .unwrap();
    let x = Commit::with_timestamp(
        "feature-x".into(),
        Some(m0.hash.clone()),
        None,
        manifest.clone(),
        "<remote>".into(),
        Some("x".into()),
        Vec::new(),
        at(1),
    )
    .unwrap();
    let m1 = Commit::with_timestamp(
        "main".into(),
        Some(m0.hash.clone()),
        Some(x.hash.clone()),
        manifest.clone(),
        "<remote>".into(),
        Some("m1 squash x".into()),
        Vec::new(),
        at(2),
    )
    .unwrap();
    let m2 = Commit::with_timestamp(
        "main".into(),
        Some(m1.hash.clone()),
        None,
        manifest.clone(),
        "<remote>".into(),
        Some("m2".into()),
        Vec::new(),
        at(3),
    )
    .unwrap();

    // Local main = M0 with complete (root) history.
    repo.store_commit(&m0).unwrap();
    if repo.get_branch("main").unwrap().is_none() {
        repo.store_branch(&Branch::new("main".into(), None, None))
            .unwrap();
    }
    repo.set_branch_head("main", &m0.hash).unwrap();
    // Branch `work` parented to main, forked at M0.
    repo.store_branch(&Branch::new("work".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("work", &m0.hash).unwrap();
    repo.set_current_branch("work").unwrap();
    repo.set_head(&m0.hash).unwrap();
    drop(repo);
    std::fs::write(dir.join("tracked.txt"), "work change\n").unwrap();
    assert_ok(&oak(dir, &["commit"]));

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();

    let mut fetch = |h: &Hash| -> oak_core::Result<oak_core::Tree> {
        repo.get_tree(h)?
            .ok_or_else(|| oak_core::OakError::ManifestNotFound(h.to_string()))
    };
    let trees: Vec<Value> = oak_core::collect_tree_objects(&manifest, &mut fetch)
        .unwrap()
        .iter()
        .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
        .collect();
    let entries: Vec<Value> = [&m0, &x, &m1, &m2].into_iter().map(info).collect();
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "head": m2.hash.to_string() })),
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

    Fx {
        temp,
        _server: server,
        m0: m0.hash,
        x_tip: x.hash,
        m2: m2.hash,
    }
}

fn fork_point(dir: &Path) -> Value {
    let out = oak(dir, &["status", "--json"]);
    assert_ok(&out);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    json["branch_changes"].clone()
}

/// RED on the candidate: `agent state --refresh` (HeadOnly) moves local main
/// to M2 but leaves X missing, so the contribution diff (fork point M0) that
/// the pre-change Complete refresh made available is now unavailable.
#[tokio::test]
async fn qa1_agent_state_refresh_keeps_local_fork_point_resolvable() {
    let fx = fixture().await;
    let dir = fx.temp.path();
    let out = oak(dir, &["agent", "state", "--json", "--compact", "--refresh"]);
    assert_ok(&out);
    eprintln!("agent state: {}", String::from_utf8_lossy(&out.stdout));
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(repo.get_branch_head("main").unwrap(), Some(fx.m2.clone()));
    let x_local = repo.get_commit(&fx.x_tip).unwrap().is_some();
    drop(repo);

    let changes = fork_point(dir);
    let merge = oak(dir, &["merge", "--dry-run", "--json"]);
    let diff = oak(dir, &["diff", "work", "--json"]);
    eprintln!(
        "diff work --json rc={:?} stdout: {}",
        diff.status.code(),
        String::from_utf8_lossy(&diff.stdout)
            .chars()
            .take(600)
            .collect::<String>()
    );
    eprintln!(
        "diff work stderr: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    eprintln!("x_tip local after refresh: {x_local}");
    eprintln!("status branch_changes: {changes}");
    eprintln!(
        "merge --dry-run stdout: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    eprintln!(
        "merge --dry-run stderr: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(
        changes["caveats"].as_array().is_none_or(|c| c.is_empty()),
        "fork point M0 {} should stay resolvable after agent state --refresh: {changes}",
        fx.m0.short()
    );
}

/// Control: the Complete path (`oak fetch`, i.e. what agent state --refresh
/// did before the candidate) leaves the fork point resolvable.
#[tokio::test]
async fn qa1_control_complete_refresh_resolves_fork_point() {
    let fx = fixture().await;
    let dir = fx.temp.path();
    assert_ok(&oak(dir, &["fetch"]));
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(repo.get_branch_head("main").unwrap(), Some(fx.m2.clone()));
    assert!(repo.get_commit(&fx.x_tip).unwrap().is_some());
    drop(repo);
    let changes = fork_point(dir);
    eprintln!("status branch_changes: {changes}");
    assert!(
        changes["caveats"].as_array().is_none_or(|c| c.is_empty()),
        "{changes}"
    );
}

/// RED on the candidate: `oak switch -c` (HeadOnly) moves the shared local
/// `main` ref for every other branch in the checkout; the pre-existing
/// branch `work` loses its resolvable contribution diff.
#[tokio::test]
async fn qa1_switch_create_keeps_other_branch_contribution_diff() {
    let fx = fixture().await;
    let dir = fx.temp.path();
    assert_ok(&oak(dir, &["switch", "-c", "next"]));
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    assert_eq!(repo.get_branch_head("main").unwrap(), Some(fx.m2.clone()));
    drop(repo);
    let diff = oak(dir, &["diff", "work", "--json"]);
    eprintln!(
        "switch-c then diff work rc={:?} {}",
        diff.status.code(),
        String::from_utf8_lossy(&diff.stdout)
            .chars()
            .take(300)
            .collect::<String>()
    );
    assert_eq!(diff.status.code(), Some(0));
}
