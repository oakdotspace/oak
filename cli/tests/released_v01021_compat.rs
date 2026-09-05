//! Executable compatibility gates against the pinned v0.102.1 release binary.
//!
//! These are ignored in ordinary CI because the released artifact is not part
//! of the source tree. Release QA runs them with `OAK_RELEASED_V01021_BIN` set
//! to the checksum-verified binary.
//!
//! The missing-parent characterization is also a rollout gate: a new client
//! cannot delegate edge validation to this released server when
//! `/commits/info` is unavailable. The fixed server must deploy before the
//! client hardening is released generally.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use oak_core::protocol::{BranchPushData, CommitData, CreateRepoRequest, PushRequest};
use oak_core::{Commit, Hash, Tree};
use tempfile::TempDir;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wire_commit(commit: &Commit) -> CommitData {
    CommitData {
        hash: commit.hash.to_string(),
        branch_name: commit.branch_name.clone(),
        parent_hash: commit.parent_hash.as_ref().map(ToString::to_string),
        merge_parent_hash: commit.merge_parent_hash.as_ref().map(ToString::to_string),
        manifest_hash: commit.manifest_hash.to_string(),
        author: commit.author.clone(),
        message: commit.message.clone(),
        timestamp: commit.timestamp.to_rfc3339(),
        files: Vec::new(),
    }
}

fn branch(name: &str) -> BranchPushData {
    BranchPushData {
        name: name.to_string(),
        description: Some("released compatibility gate".to_string()),
        parent_branch: Some("main".to_string()),
        status: "open".to_string(),
        created_at: Utc::now().to_rfc3339(),
        close_reason: None,
    }
}

fn push_request(commit: &Commit, include_empty_tree: bool) -> PushRequest {
    let tree = Tree::new(Vec::new()).unwrap();
    PushRequest {
        expected_head: None,
        expected_branch_head: None,
        force: false,
        branch: Some(branch(&commit.branch_name)),
        commits: vec![wire_commit(commit)],
        blobs: Vec::new(),
        trees: include_empty_tree
            .then(|| oak_core::protocol::tree_to_wire(&tree))
            .into_iter()
            .collect(),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the checksum-pinned v0.102.1 release artifact"]
async fn released_v01021_serve_accepts_stored_but_does_not_reject_missing_external_edges() {
    oak_cli::http::ensure_crypto_provider();
    let binary = std::env::var("OAK_RELEASED_V01021_BIN")
        .expect("set OAK_RELEASED_V01021_BIN to the pinned v0.102.1 artifact");
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "oak 0.102.1"
    );

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let root = TempDir::new().unwrap();
    let child = Command::new(&binary)
        .args([
            "serve",
            "--dir",
            root.path().to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _child = ChildGuard(child);
    let base_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..100 {
        if client
            .get(format!("{base_url}/api/capabilities"))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "released oak serve did not become ready");

    client
        .post(format!("{base_url}/api/repos"))
        .json(&CreateRepoRequest {
            name: "graph-gate".to_string(),
            description: None,
            is_public: true,
            organization_slug: Some("oak".to_string()),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let unpublished_branch_pull: serde_json::Value = client
        .get(format!(
            "{base_url}/api/oak/graph-gate/pull?branch_name=never-published"
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unpublished_branch_pull["head"], serde_json::Value::Null);
    assert_eq!(unpublished_branch_pull["branch"], serde_json::Value::Null);
    assert_eq!(unpublished_branch_pull["branches"], serde_json::json!([]));
    assert_eq!(unpublished_branch_pull["commits"], serde_json::json!([]));
    assert_eq!(unpublished_branch_pull["blobs"], serde_json::json!([]));
    assert_eq!(unpublished_branch_pull["trees"], serde_json::json!([]));

    let manifest = Tree::empty_hash();
    let now = Utc::now();
    let base = Commit::with_timestamp(
        "base".to_string(),
        None,
        None,
        manifest.clone(),
        "qa".to_string(),
        None,
        Vec::new(),
        now,
    )
    .unwrap();
    client
        .post(format!("{base_url}/api/oak/graph-gate/push"))
        .json(&push_request(&base, true))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let valid_external = Commit::with_timestamp(
        "valid-external".to_string(),
        Some(base.hash.clone()),
        None,
        manifest.clone(),
        "qa".to_string(),
        None,
        Vec::new(),
        now + TimeDelta::seconds(1),
    )
    .unwrap();
    client
        .post(format!("{base_url}/api/oak/graph-gate/push"))
        .json(&push_request(&valid_external, false))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let published: serde_json::Value = client
        .get(format!(
            "{base_url}/api/oak/graph-gate/branches/valid-external"
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["head"], valid_external.hash.as_str());

    let missing = Hash("ab".repeat(32));
    let orphan = Commit::with_timestamp(
        "missing-external".to_string(),
        Some(missing),
        None,
        manifest,
        "qa".to_string(),
        None,
        Vec::new(),
        now + TimeDelta::seconds(2),
    )
    .unwrap();
    let rejected = client
        .post(format!("{base_url}/api/oak/graph-gate/push"))
        .json(&push_request(&orphan, false))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::OK);
    let unpublished: serde_json::Value = client
        .get(format!(
            "{base_url}/api/oak/graph-gate/branches/missing-external"
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unpublished["head"], orphan.hash.as_str());
}
