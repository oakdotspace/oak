//! Parent (`main`) refresh cost in a shallow worker (fb-526).
//!
//! A `oak clone --shallow` worker holds main's head commit and snapshot but
//! none of its ancestors. Commands that only need main's *head* (`oak switch
//! -c`, `oak agent state --refresh`) must not walk the whole ancestry chain one
//! `POST /commits/info` per commit; commands that compute merge bases or that
//! are advertised as the local-history repair (`oak fetch`, `oak pull`) must
//! still backfill the complete chain exactly as before.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, Commit, Hash, MetadataKey, Repository, SqliteRepository};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Depth of the remote-only history below the shallow boundary.
const ANCESTORS: usize = 60;

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

fn commit_info_json(commit: &Commit) -> Value {
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

struct ShallowFixture {
    _temp: tempfile::TempDir,
    head: Hash,
    ancestors: Vec<Hash>,
    server: MockServer,
}

impl ShallowFixture {
    fn dir(&self) -> &Path {
        self._temp.path()
    }

    fn repo(&self) -> SqliteRepository {
        SqliteRepository::open(&self.dir().join(".oak/oak.db")).unwrap()
    }

    async fn commit_info_requests(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path().ends_with("/commits/info"))
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    fn local_ancestor_rows(&self) -> usize {
        let repo = self.repo();
        self.ancestors
            .iter()
            .filter(|hash| repo.get_commit(hash).unwrap().is_some())
            .count()
    }
}

/// A checkout in the exact state `oak clone --shallow` leaves: main's head
/// commit row + hydrated snapshot local, `ANCESTORS` first-parent ancestors
/// (each also carrying a merge-parent edge, like real squash merges) known
/// only to the server, and no `main_last_checked_at` stamp.
async fn shallow_fixture() -> ShallowFixture {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    assert_ok(&oak(dir, &["init", "."]));
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert_ok(&oak(dir, &["commit"]));

    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let local_head = repo.get_branch_head(&branch).unwrap().unwrap();
    let manifest_hash = repo.get_commit(&local_head).unwrap().unwrap().manifest_hash;

    let at = |i: usize| chrono::DateTime::from_timestamp(1_700_000_000 + i as i64, 0).unwrap();
    let mut remote_commits = Vec::new();
    let mut parent: Option<Hash> = None;
    for i in 0..ANCESTORS {
        // A merged feature tip: main's squash commits carry it as merge parent.
        let feature = Commit::with_timestamp(
            format!("feature-{i}"),
            parent.clone(),
            None,
            manifest_hash.clone(),
            "<remote>".into(),
            Some(format!("feature {i}")),
            Vec::new(),
            at(i),
        )
        .unwrap();
        let main = Commit::with_timestamp(
            "main".into(),
            parent.clone(),
            Some(feature.hash.clone()),
            manifest_hash.clone(),
            "<remote>".into(),
            Some(format!("main {i}")),
            Vec::new(),
            at(i),
        )
        .unwrap();
        parent = Some(main.hash.clone());
        remote_commits.push(feature);
        remote_commits.push(main);
    }
    let head = Commit::with_timestamp(
        "main".into(),
        parent,
        None,
        manifest_hash.clone(),
        "<remote>".into(),
        Some("main head".into()),
        Vec::new(),
        at(ANCESTORS),
    )
    .unwrap();

    repo.store_commit(&head).unwrap();
    if repo.get_branch("main").unwrap().is_none() {
        repo.store_branch(&Branch::new("main".into(), None, None))
            .unwrap();
    }
    repo.set_branch_head("main", &head.hash).unwrap();
    repo.set_branch_head(&branch, &head.hash).unwrap();
    repo.set_head(&head.hash).unwrap();

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
    let trees: Vec<Value> = oak_core::collect_tree_objects(&manifest_hash, &mut fetch)
        .unwrap()
        .iter()
        .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
        .collect();
    let mut entries: Vec<Value> = remote_commits.iter().map(commit_info_json).collect();
    entries.push(commit_info_json(&head));

    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "head": head.hash.to_string() })),
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
        .and(path(format!("/api/oak/oak/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let ancestors = remote_commits.iter().map(|c| c.hash.clone()).collect();
    ShallowFixture {
        _temp: temp,
        head: head.hash,
        ancestors,
        server,
    }
}

#[tokio::test]
async fn switch_create_in_shallow_worker_needs_only_mains_head() {
    let fx = shallow_fixture().await;

    let out = oak(fx.dir(), &["switch", "-c", "next"]);
    assert_ok(&out);

    let repo = fx.repo();
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("next")
    );
    assert_eq!(repo.get_branch_head("next").unwrap(), Some(fx.head.clone()));
    assert_eq!(repo.get_head().unwrap(), Some(fx.head.clone()));
    assert_eq!(repo.get_branch_head("main").unwrap(), Some(fx.head.clone()));
    assert_eq!(
        std::fs::read_to_string(fx.dir().join("tracked.txt")).unwrap(),
        "base\n"
    );
    let requests = fx.commit_info_requests().await;
    assert_eq!(
        requests.len(),
        0,
        "switch -c must not walk main's ancestry: {} commits/info request(s)",
        requests.len()
    );
    assert_eq!(fx.local_ancestor_rows(), 0);
}

#[tokio::test]
async fn agent_state_refresh_in_shallow_worker_needs_only_mains_head() {
    let fx = shallow_fixture().await;

    let out = oak(
        fx.dir(),
        &["agent", "state", "--json", "--compact", "--refresh"],
    );
    assert_ok(&out);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["remote_parent_head"], fx.head.to_string());

    let requests = fx.commit_info_requests().await;
    assert_eq!(
        requests.len(),
        0,
        "agent state --refresh must not walk main's ancestry: {} commits/info request(s)",
        requests.len()
    );
    assert_eq!(fx.local_ancestor_rows(), 0);
}

/// `oak fetch` is the advertised repair for incomplete local lineage (review,
/// diff and merge errors name it), so it keeps today's complete backfill —
/// including after a head-only `switch -c` left the chain unfilled — and its
/// ancestor requests never ask for tree objects they would discard.
#[tokio::test]
async fn fetch_after_head_only_switch_still_repairs_complete_ancestry() {
    let fx = shallow_fixture().await;
    assert_ok(&oak(fx.dir(), &["switch", "-c", "next"]));
    assert_eq!(fx.local_ancestor_rows(), 0);

    let out = oak(fx.dir(), &["fetch"]);
    assert_ok(&out);

    assert_eq!(
        fx.local_ancestor_rows(),
        fx.ancestors.len(),
        "fetch must link every ancestor, merge parents included"
    );
    let requests = fx.commit_info_requests().await;
    assert_eq!(requests.len(), 1 + fx.ancestors.len());
    for request in &requests {
        assert_eq!(
            request["metadata_only"], true,
            "ancestor/repair requests discard trees and must not ask for them: {request}"
        );
    }
    let repo = fx.repo();
    assert_eq!(
        repo.get_current_branch_name().unwrap().as_deref(),
        Some("next")
    );
    assert_eq!(repo.get_branch_head("next").unwrap(), Some(fx.head.clone()));
}
