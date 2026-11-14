//! Pushes above the ordinary 128-blob / 64 MiB envelope against a server
//! that advertises `ordinary_bootstrap_protocol: headless_preload_v1` but
//! not `staged_v1` (fb-550).
//!
//! These tests run the real client against a stateful stand-in for the
//! hosted server's ordinary `/push` handler: a request without branch metadata
//! and without commits stores verified content-addressed blobs and nothing
//! else, guarded by the repository head; a request with commits is one
//! branch-head CAS.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use oak_cli::output;
use oak_core::{
    Branch, ChangeType, FileChange, FileMode, Hash, ManifestEntry, MetadataKey, OakError,
    Repository, SqliteRepository,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const OWNER: &str = "acme";
const REPO: &str = "demo";

fn headless_capabilities() -> Value {
    json!({
        "push_protocol": "legacy",
        "staged_session_protocol": "opaque_v1",
        "staged_capabilities_ready": false,
        "staged_abort_protocol": "v1",
        "known_loss_protocol": "report_v1",
        "ordinary_bootstrap_protocol": "headless_preload_v1"
    })
}

fn legacy_capabilities() -> Value {
    json!({"push_protocol": "legacy", "staged_capabilities_ready": false})
}

/// One `/push` request as the stand-in saw it.
#[derive(Clone, Debug)]
struct PushRecord {
    commits: usize,
    blobs: usize,
    trees: usize,
    had_branch: bool,
    /// Whether any branch row or branch head other than the seeded ones
    /// existed when this request arrived.
    branch_state_changed_before: bool,
}

#[derive(Default)]
struct Hosted {
    capabilities: Value,
    repo_head: Option<String>,
    branch_heads: HashMap<String, String>,
    branch_rows: HashSet<String>,
    blobs: HashMap<String, Vec<u8>>,
    chunked_blobs: HashMap<String, Value>,
    pushes: Vec<PushRecord>,
    head_moves: usize,
    capability_probes: usize,
    preload_requests: usize,
    /// Answer this (1-based) preload request with HTTP 500.
    fail_preload_request: Option<usize>,
    /// Answer every preload request with this status.
    preload_refusal_status: Option<u16>,
    /// Refuse the repository-head guard of every preload request.
    refuse_preload_guard: bool,
    /// Answer the publication request with this status instead.
    publication_status: Option<u16>,
    /// A concurrent push lands on the branch just before publication.
    concurrent_branch_head: Option<String>,
    /// `main` moves once before the next preload request is checked.
    concurrent_repo_head: Option<String>,
    seeded_branch_rows: HashSet<String>,
    seeded_branch_heads: HashMap<String, String>,
    /// Statuses scripted per preload arrival ordinal (1-based).
    scripted_preload_status: HashMap<usize, u16>,
    /// Every preload arrival from this ordinal on answers this status.
    preload_status_from: Option<(usize, u16)>,
    /// Delay of every `/push` response.
    push_delay_ms: u64,
    /// Arrival instant of every preload request.
    preload_arrivals: Vec<Instant>,
    /// Answer `scripted_preload_status` immediately instead of after
    /// `push_delay_ms`, so the scripted failure is observed while its
    /// siblings are still in flight.
    scripted_status_immediate: bool,
    /// Whether the response being built came from `scripted_preload_status`.
    answered_scripted: bool,
}

impl Hosted {
    fn branch_state_changed(&self) -> bool {
        self.branch_rows != self.seeded_branch_rows || self.branch_heads != self.seeded_branch_heads
    }

    fn push(&mut self, body: &Value) -> ResponseTemplate {
        self.answered_scripted = false;
        let commits = body["commits"].as_array().cloned().unwrap_or_default();
        let blobs = body["blobs"].as_array().cloned().unwrap_or_default();
        let trees = body["trees"].as_array().map_or(0, Vec::len);
        let branch = body["branch"]["name"].as_str().map(str::to_string);
        self.pushes.push(PushRecord {
            commits: commits.len(),
            blobs: blobs.len(),
            trees,
            had_branch: branch.is_some(),
            branch_state_changed_before: self.branch_state_changed(),
        });
        let conflict = |head: Option<&String>| {
            ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "new_head": head,
                "message": "Conflict: remote head has changed. Pull first."
            }))
        };

        if commits.is_empty() && branch.is_none() {
            // Headless preload: guarded by the repository head, stores blobs.
            self.preload_requests += 1;
            self.preload_arrivals.push(Instant::now());
            if let Some(status) = self.scripted_preload_status.remove(&self.preload_requests) {
                self.answered_scripted = true;
                return ResponseTemplate::new(status).set_body_string("scripted status");
            }
            if let Some((from, status)) = self.preload_status_from {
                if self.preload_requests >= from {
                    return ResponseTemplate::new(status).set_body_string("scripted status");
                }
            }
            if self.fail_preload_request == Some(self.preload_requests) {
                return ResponseTemplate::new(500).set_body_string("injected preload failure");
            }
            if let Some(status) = self.preload_refusal_status {
                return ResponseTemplate::new(status).set_body_string("injected preload refusal");
            }
            if self.refuse_preload_guard {
                return conflict(self.repo_head.as_ref());
            }
            if let Some(moved) = self.concurrent_repo_head.take() {
                self.repo_head = Some(moved);
            }
            if body["expected_head"].as_str().map(str::to_string) != self.repo_head {
                return conflict(self.repo_head.as_ref());
            }
            if let Err(error) = self.store_blobs(&blobs) {
                return ResponseTemplate::new(400).set_body_string(error);
            }
            return ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "new_head": self.repo_head, "message": "ok"
            }));
        }
        let Some(branch) = branch else {
            return ResponseTemplate::new(400).set_body_string("commits without branch metadata");
        };
        if commits.is_empty() {
            // Metadata-only: this is what makes a branch row visible.
            self.branch_rows.insert(branch);
            return ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "new_head": self.repo_head, "message": "ok"
            }));
        }
        if let Some(status) = self.publication_status {
            return ResponseTemplate::new(status).set_body_string("injected publication failure");
        }
        if let Some(head) = self.concurrent_branch_head.take() {
            self.branch_heads.insert(branch.clone(), head);
            self.head_moves += 1;
        }
        let current = self.branch_heads.get(&branch).cloned();
        let first_parent = commits[0]["parent_hash"].as_str().map(str::to_string);
        if current.is_some() && current != first_parent {
            return conflict(current.as_ref());
        }
        if let Err(error) = self.store_blobs(&blobs) {
            return ResponseTemplate::new(400).set_body_string(error);
        }
        let tip = commits.last().unwrap()["hash"]
            .as_str()
            .unwrap()
            .to_string();
        self.branch_rows.insert(branch.clone());
        self.branch_heads.insert(branch, tip.clone());
        self.head_moves += 1;
        ResponseTemplate::new(200)
            .set_body_json(json!({"success": true, "new_head": tip, "message": "ok"}))
    }

    fn store_blobs(&mut self, blobs: &[Value]) -> Result<(), String> {
        // Validate the whole envelope before storing any of it.
        let mut inline = Vec::new();
        for blob in blobs {
            let hash = blob["hash"].as_str().unwrap().to_string();
            if blob["chunks"]
                .as_array()
                .is_some_and(|chunks| !chunks.is_empty())
            {
                continue;
            }
            let content: Vec<u8> = blob["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| byte.as_u64().unwrap() as u8)
                .collect();
            if oak_core::hash_bytes(&content).as_str() != hash
                || blob["size"].as_u64() != Some(content.len() as u64)
            {
                return Err(format!(
                    "inline blob {hash} does not match its content hash"
                ));
            }
            inline.push((hash, content));
        }
        self.blobs.extend(inline);
        for blob in blobs {
            if blob["chunks"]
                .as_array()
                .is_some_and(|chunks| !chunks.is_empty())
            {
                self.chunked_blobs
                    .insert(blob["hash"].as_str().unwrap().to_string(), blob.clone());
            }
        }
        Ok(())
    }
}

type Shared = Arc<Mutex<Hosted>>;

async fn hosted(capabilities: Value, repo_head: Option<&Hash>) -> (MockServer, Shared) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state: Shared = Arc::new(Mutex::new(Hosted {
        capabilities,
        repo_head: repo_head.map(ToString::to_string),
        ..Hosted::default()
    }));
    let server = MockServer::start().await;
    let shared = state.clone();
    Mock::given(method("GET"))
        .and(path("/api/capabilities"))
        .respond_with(move |_: &Request| {
            let mut hosted = shared.lock().unwrap();
            hosted.capability_probes += 1;
            ResponseTemplate::new(200).set_body_json(hosted.capabilities.clone())
        })
        .mount(&server)
        .await;
    let shared = state.clone();
    Mock::given(method("GET"))
        .and(path(format!("/api/{OWNER}/{REPO}")))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"head": shared.lock().unwrap().repo_head}))
        })
        .mount(&server)
        .await;
    let shared = state.clone();
    Mock::given(method("GET"))
        .and(path_regex(format!("^/api/{OWNER}/{REPO}/branches/[^/]+$")))
        .respond_with(move |request: &Request| {
            let name = request.url.path().rsplit('/').next().unwrap().to_string();
            match shared.lock().unwrap().branch_heads.get(&name) {
                Some(head) => ResponseTemplate::new(200).set_body_json(json!({"head": head})),
                None => ResponseTemplate::new(404),
            }
        })
        .mount(&server)
        .await;
    let shared = state.clone();
    Mock::given(method("POST"))
        .and(path(format!("/api/{OWNER}/{REPO}/blobs/check")))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let hosted = shared.lock().unwrap();
            let missing: Vec<&Value> = body["hashes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|hash| {
                    let hash = hash.as_str().unwrap();
                    !hosted.blobs.contains_key(hash) && !hosted.chunked_blobs.contains_key(hash)
                })
                .collect();
            ResponseTemplate::new(200)
                .set_body_json(json!({"missing": missing, "verified_content": false}))
        })
        .mount(&server)
        .await;
    // Every chunk is reported present: these tests pin the publication
    // requests, and chunk upload is the unchanged ordinary pipeline.
    Mock::given(method("POST"))
        .and(path(format!("/api/{OWNER}/{REPO}/chunks/check")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"missing": []})))
        .mount(&server)
        .await;
    let shared = state.clone();
    Mock::given(method("POST"))
        .and(path(format!("/api/{OWNER}/{REPO}/push")))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let mut hosted = shared.lock().unwrap();
            let response = hosted.push(&body);
            let delay = if hosted.answered_scripted && hosted.scripted_status_immediate {
                0
            } else {
                hosted.push_delay_ms
            };
            response.set_delay(Duration::from_millis(delay))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(format!("^/api/{OWNER}/{REPO}/push/staged")))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    (server, state)
}

struct Seed {
    base: Hash,
    tip: Hash,
    /// Every file the outgoing commits add: blob hash and exact bytes.
    files: Vec<(Hash, Vec<u8>)>,
}

/// A branch whose base commit (an empty tree) stands for what the server
/// already has, followed by one commit per entry of `files_per_commit`.
fn seed_branch(repo: &SqliteRepository, branch: &str, files_per_commit: &[usize]) -> Seed {
    repo.store_branch(&Branch::new(
        branch.to_string(),
        Some("blob preload".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let base = repo
        .put_commit(
            branch.to_string(),
            None,
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut parent = base.clone();
    for (commit_index, count) in files_per_commit.iter().enumerate() {
        let mut changes = Vec::new();
        for index in 0..*count {
            let bytes = format!("{branch} commit {commit_index} file {index}\n").into_bytes();
            let hash = repo.put_blob(bytes.clone()).unwrap();
            let path = format!("c{commit_index}/f{index:04}.txt");
            entries.push(ManifestEntry {
                path: path.clone(),
                blob_hash: hash.clone(),
                mode: FileMode::Regular,
            });
            changes.push(FileChange {
                path,
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(hash.clone()),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            });
            files.push((hash, bytes));
        }
        let manifest = repo.put_manifest(entries.clone()).unwrap();
        parent = repo
            .put_commit(
                branch.to_string(),
                Some(parent),
                None,
                manifest,
                "tester".to_string(),
                None,
                Utc::now(),
                changes,
            )
            .unwrap();
    }
    repo.set_branch_head(branch, &parent).unwrap();
    repo.set_head(&parent).unwrap();
    Seed {
        base,
        tip: parent,
        files,
    }
}

fn publish_base(state: &Shared, branch: &str, base: &Hash) {
    let mut hosted = state.lock().unwrap();
    hosted
        .branch_heads
        .insert(branch.to_string(), base.to_string());
    hosted.branch_rows.insert(branch.to_string());
    hosted.seeded_branch_heads = hosted.branch_heads.clone();
    hosted.seeded_branch_rows = hosted.branch_rows.clone();
}

async fn push(
    repo: &SqliteRepository,
    dir: &Path,
    server: &MockServer,
    branch: &str,
) -> oak_core::Result<()> {
    output::begin_capture();
    let result = oak_cli::commands::push::push_async(
        repo,
        dir,
        &server.uri(),
        OWNER,
        REPO,
        Some(branch),
        false,
        None,
    )
    .await;
    output::end_capture();
    result
}

/// The server holds byte-identical content for every pushed file, the head
/// moved exactly once to the local tip, and nothing about any branch was
/// visible before the one publication request.
fn assert_published_once(state: &Shared, branch: &str, seed: &Seed, commits: usize) {
    let hosted = state.lock().unwrap();
    for (hash, bytes) in &seed.files {
        assert_eq!(
            hosted.blobs.get(hash.as_str()),
            Some(bytes),
            "server content for {hash} must be byte-identical"
        );
    }
    assert_eq!(hosted.branch_heads.get(branch), Some(&seed.tip.to_string()));
    assert_eq!(hosted.head_moves, 1, "exactly one branch-head move");
    let (publication, preloads) = hosted.pushes.split_last().unwrap();
    assert_eq!(publication.commits, commits);
    assert!(publication.had_branch);
    assert_eq!(
        publication.blobs, 0,
        "publication carries no preloaded blob"
    );
    assert!(publication.trees > 0, "publication carries its own trees");
    assert!(!preloads.is_empty());
    for preload in preloads {
        assert_eq!((preload.commits, preload.trees), (0, 0), "{preload:?}");
        assert!(
            !preload.had_branch,
            "a preload names no branch: {preload:?}"
        );
        assert!(preload.blobs >= 1 && preload.blobs <= 128, "{preload:?}");
    }
    assert!(
        hosted
            .pushes
            .iter()
            .all(|push| !push.branch_state_changed_before),
        "no branch row or head may change before the publication: {:?}",
        hosted.pushes
    );
}

#[tokio::test(flavor = "current_thread")]
async fn existing_branch_push_preloads_129_400_and_1000_new_blobs_then_publishes_once() {
    for (files, preload_requests) in [(129usize, 2usize), (400, 4), (1000, 8)] {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let branch = format!("feature-{files}");
        let seed = seed_branch(&repo, &branch, &[files]);
        let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
        publish_base(&state, &branch, &seed.base);

        push(&repo, temp.path(), &server, &branch)
            .await
            .unwrap_or_else(|error| panic!("{files} new files must publish: {error}"));

        assert_published_once(&state, &branch, &seed, 1);
        let hosted = state.lock().unwrap();
        assert_eq!(hosted.preload_requests, preload_requests);
        assert_eq!(hosted.pushes.len(), preload_requests + 1);
        assert_eq!(
            hosted.pushes[..preload_requests]
                .iter()
                .map(|push| push.blobs)
                .sum::<usize>(),
            files
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn new_branch_on_existing_repo_is_invisible_until_the_one_publication() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-new-branch";
    let seed = seed_branch(&repo, branch, &[129]);
    // The repository exists (it has a main head) but this branch does not.
    let main_head = Hash::from_hex(&"ab".repeat(32)).unwrap();
    let (server, state) = hosted(headless_capabilities(), Some(&main_head)).await;

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("a new branch above the envelope must publish");

    // The base commit is outgoing too: the server has no head for the branch.
    assert_published_once(&state, branch, &seed, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn two_commits_of_70_files_preload_once_and_publish_together() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-two-commits";
    let seed = seed_branch(&repo, branch, &[70, 70]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("2 x 70 new files must publish");

    assert_published_once(&state, branch, &seed, 2);
    assert_eq!(state.lock().unwrap().preload_requests, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn server_without_headless_preload_gets_the_dedicated_refusal_before_any_mutation() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-legacy";
    let seed = seed_branch(&repo, branch, &[129]);
    let (server, state) = hosted(legacy_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);

    let error = push(&repo, temp.path(), &server, branch)
        .await
        .expect_err("a legacy server cannot take 129 new blobs");

    let OakError::PushCapabilityUnavailable {
        message,
        recommended_next_commands,
    } = &error
    else {
        panic!("expected the dedicated capability refusal, got {error:?}");
    };
    for needle in [
        "129 new blobs > 128",
        "requires staged_v1",
        "does not advertise ordinary_bootstrap_protocol=headless_preload_v1",
        "no remote state was mutated",
    ] {
        assert!(message.contains(needle), "missing {needle:?} in: {message}");
    }
    assert_eq!(recommended_next_commands, &["oak push --plan --json"]);
    let hosted = state.lock().unwrap();
    assert!(hosted.pushes.is_empty(), "no mutating request was sent");
    assert_eq!(hosted.blobs.len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn commit_count_above_the_envelope_is_not_lifted_by_the_blob_preload() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-many-commits";
    let seed = seed_branch(&repo, branch, &vec![0; 501]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);

    let error = push(&repo, temp.path(), &server, branch)
        .await
        .expect_err("501 commits still need staged_v1");

    let message = error.to_string();
    assert!(
        matches!(error, OakError::PushCapabilityUnavailable { .. }),
        "{error:?}"
    );
    assert!(message.contains("501 outgoing commits > 500"), "{message}");
    assert!(
        message.contains("lifts only the new-blob count and byte limits"),
        "{message}"
    );
    assert!(message.contains("no remote state was mutated"), "{message}");
    assert!(state.lock().unwrap().pushes.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_preload_reports_truthfully_and_the_retry_sends_only_missing_blobs() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-interrupted";
    let seed = seed_branch(&repo, branch, &[400]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().fail_preload_request = Some(2);

    let error = push(&repo, temp.path(), &server, branch)
        .await
        .expect_err("the injected failure stops the preload");
    let message = error.to_string();
    assert!(matches!(error, OakError::Server(_)), "{error:?}");
    for needle in [
        "blob preload request ",
        "/4 failed",
        "HTTP 500",
        "No branch head was moved and nothing was published",
        " of 400 new blob(s) are stored",
        "Retry `oak push`",
    ] {
        assert!(message.contains(needle), "missing {needle:?} in: {message}");
    }
    assert!(
        !message.contains("injected"),
        "server bodies stay out of errors"
    );
    // After the first request the rest run concurrently: the failed request
    // is whichever arrived second, and requests already in flight finish.
    // The reported count must be exactly what the server stored.
    let reported: usize = message
        .split(" of 400 new blob(s) are stored")
        .next()
        .and_then(|head| head.rsplit(' ').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no stored count in: {message}"));
    let stored = {
        let mut hosted = state.lock().unwrap();
        assert_eq!(hosted.head_moves, 0);
        assert!(!hosted.branch_state_changed());
        assert_eq!(hosted.blobs.len(), reported, "{message}");
        assert!((16..400).contains(&reported), "{message}");
        hosted.fail_preload_request = None;
        hosted.pushes.clear();
        reported
    };

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("the retry completes the publication");

    // What is left may fit one ordinary publication (no second preload), so
    // only the outcome is pinned: byte-identical content, one head move.
    let hosted = state.lock().unwrap();
    for (hash, bytes) in &seed.files {
        assert_eq!(hosted.blobs.get(hash.as_str()), Some(bytes));
    }
    assert_eq!(hosted.branch_heads.get(branch), Some(&seed.tip.to_string()));
    assert_eq!(hosted.head_moves, 1, "exactly one branch-head move");
    assert!(
        hosted
            .pushes
            .iter()
            .all(|push| !push.branch_state_changed_before),
        "no branch row or head may change before the publication: {:?}",
        hosted.pushes
    );
    let (publication, earlier) = hosted.pushes.split_last().unwrap();
    assert!(publication.had_branch && publication.commits == 1);
    assert!(earlier
        .iter()
        .all(|push| push.commits == 0 && !push.had_branch));
    let resent: usize = hosted.pushes.iter().map(|push| push.blobs).sum();
    assert_eq!(
        resent,
        400 - stored,
        "only the blobs still missing are sent again"
    );
}

/// A server that refuses the preload itself (an external contributor's 403,
/// a handler without the non-empty-repository behaviour) is a capability
/// outcome: no retry advice, no login prompt, nothing about a branch changed.
#[tokio::test(flavor = "current_thread")]
async fn refused_preload_is_a_capability_refusal_without_retry_or_login_advice() {
    for status in [403u16, 400, 422] {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let branch = format!("feature-refused-{status}");
        let seed = seed_branch(&repo, &branch, &[129]);
        let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
        publish_base(&state, &branch, &seed.base);
        state.lock().unwrap().preload_refusal_status = Some(status);

        let error = push(&repo, temp.path(), &server, &branch)
            .await
            .expect_err("the server refuses the preload");

        let OakError::PushCapabilityUnavailable {
            message,
            recommended_next_commands,
        } = &error
        else {
            panic!("{status}: expected the capability refusal, got {error:?}");
        };
        let detail = format!("HTTP {status}");
        for needle in [
            "the server refused blob preload for this repository",
            "request 1/2",
            detail.as_str(),
            "no branch state was changed",
            "0 of 129 new blob(s) were uploaded and remain unreferenced",
        ] {
            assert!(
                message.contains(needle),
                "{status}: missing {needle:?} in: {message}"
            );
        }
        for forbidden in ["Retry", "oak login", "injected"] {
            assert!(!message.contains(forbidden), "{status}: {message}");
        }
        assert_eq!(recommended_next_commands, &["oak status --json"]);
        let hosted = state.lock().unwrap();
        assert_eq!(
            hosted.preload_requests, 1,
            "a deterministic refusal is not resent"
        );
        assert_eq!(hosted.head_moves, 0);
        assert!(!hosted.branch_state_changed());
        assert!(hosted.blobs.is_empty());
        assert!(hosted
            .pushes
            .iter()
            .all(|push| push.commits == 0 && !push.had_branch));
    }
}

/// The server's "changed during push; retry" statuses and a repository head
/// that moves under every attempt are transient: resent within the preload,
/// and when they persist, an ordinary retryable server error.
#[tokio::test(flavor = "current_thread")]
async fn transient_preload_conflicts_are_retried_and_never_a_capability_refusal() {
    for transient in ["409", "423", "425", "guard"] {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let branch = format!("feature-transient-{transient}");
        let seed = seed_branch(&repo, &branch, &[129]);
        let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
        publish_base(&state, &branch, &seed.base);
        {
            let mut hosted = state.lock().unwrap();
            match transient.parse::<u16>() {
                Ok(status) => hosted.preload_refusal_status = Some(status),
                Err(_) => hosted.refuse_preload_guard = true,
            }
        }

        let error = push(&repo, temp.path(), &server, &branch)
            .await
            .expect_err("the conflict persists");

        let OakError::Server(message) = &error else {
            panic!("{transient}: expected a retryable server error, got {error:?}");
        };
        let detail = match transient {
            "guard" => "the repository head kept moving during preload".to_string(),
            status => format!("HTTP {status}"),
        };
        for needle in [
            "blob preload request 1/2 failed",
            detail.as_str(),
            "No branch head was moved and nothing was published",
            "Retry `oak push`",
        ] {
            assert!(
                message.contains(needle),
                "{transient}: missing {needle:?} in: {message}"
            );
        }
        {
            let mut hosted = state.lock().unwrap();
            assert_eq!(
                hosted.preload_requests, 4,
                "{transient}: resent within the preload"
            );
            assert_eq!(hosted.head_moves, 0);
            assert!(!hosted.branch_state_changed());
            // The conflict clears: the same push now completes.
            hosted.preload_refusal_status = None;
            hosted.refuse_preload_guard = false;
            hosted.pushes.clear();
        }
        push(&repo, temp.path(), &server, &branch)
            .await
            .unwrap_or_else(|error| panic!("{transient}: the retry publishes: {error}"));
        assert_published_once(&state, &branch, &seed, 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_publication_after_preload_moves_no_head_and_the_retry_is_one_ordinary_push() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-rejected-publication";
    let seed = seed_branch(&repo, branch, &[129]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().publication_status = Some(400);

    let error = push(&repo, temp.path(), &server, branch)
        .await
        .expect_err("the publication is rejected");
    assert!(
        matches!(&error, OakError::Server(message) if message.contains("rejected with HTTP 400")),
        "an explicit rejection is not an unconfirmed publication: {error:?}"
    );
    {
        let mut hosted = state.lock().unwrap();
        assert_eq!(hosted.head_moves, 0);
        assert!(!hosted.branch_state_changed());
        assert_eq!(hosted.blobs.len(), 129, "the preload itself completed");
        hosted.publication_status = None;
        hosted.pushes.clear();
        hosted.preload_requests = 0;
    }

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("the retry publishes");

    let hosted = state.lock().unwrap();
    assert_eq!(hosted.preload_requests, 0, "every blob is already stored");
    assert_eq!(hosted.pushes.len(), 1);
    assert_eq!((hosted.pushes[0].commits, hosted.pushes[0].blobs), (1, 0));
    assert_eq!(hosted.head_moves, 1);
    assert_eq!(hosted.branch_heads.get(branch), Some(&seed.tip.to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn ambiguous_publication_after_preload_is_unconfirmed_not_a_preload_failure() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-ambiguous-publication";
    let seed = seed_branch(&repo, branch, &[129]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().publication_status = Some(500);

    let error = push(&repo, temp.path(), &server, branch)
        .await
        .expect_err("the publication outcome is unknown");

    assert!(
        matches!(error, OakError::PublicationUnconfirmed { .. }),
        "same classification as an ordinary push: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_branch_head_move_after_preload_is_the_ordinary_conflict() {
    let mut refusals = Vec::new();
    for files in [2usize, 129] {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let branch = format!("feature-conflict-{files}");
        let seed = seed_branch(&repo, &branch, &[files]);
        let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
        publish_base(&state, &branch, &seed.base);
        let foreign = "cd".repeat(32);
        state.lock().unwrap().concurrent_branch_head = Some(foreign.clone());

        let error = push(&repo, temp.path(), &server, &branch)
            .await
            .expect_err("the branch moved under the push");

        let hosted = state.lock().unwrap();
        assert_eq!(hosted.branch_heads.get(&branch), Some(&foreign));
        assert_eq!(hosted.preload_requests, usize::from(files > 128) * 2);
        refusals.push(format!("{error:?}"));
    }
    assert_eq!(refusals[0], format!("{:?}", OakError::ConflictDetected));
    assert_eq!(
        refusals[0], refusals[1],
        "the preload does not change the refusal"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn main_moving_during_preload_is_retried_against_the_reported_head() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-main-moves";
    let seed = seed_branch(&repo, branch, &[129]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().concurrent_repo_head = Some("ef".repeat(32));

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("a moved main only re-aims the preload guard");

    let hosted = state.lock().unwrap();
    assert_eq!(hosted.preload_requests, 3, "one refused request is resent");
    assert_eq!(hosted.blobs.len(), 129);
    assert_eq!(hosted.head_moves, 1);
    assert_eq!(hosted.branch_heads.get(branch), Some(&seed.tip.to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn one_blob_above_64_mib_travels_alone_as_chunk_references() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-large-blob";
    repo.store_branch(&Branch::new(
        branch.to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let base = repo
        .put_commit(
            branch.to_string(),
            None,
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    let mut large = vec![0u8; 65 * 1024 * 1024];
    for (index, byte) in large.iter_mut().enumerate().step_by(4096) {
        *byte = (index / 4096 % 251) as u8;
    }
    let large_hash = repo.put_blob(large).unwrap();
    let small_hash = repo.put_blob(b"small neighbour\n".to_vec()).unwrap();
    let manifest = repo
        .put_manifest(vec![
            ManifestEntry {
                path: "assets/large.bin".to_string(),
                blob_hash: large_hash.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "small.txt".to_string(),
                blob_hash: small_hash.clone(),
                mode: FileMode::Regular,
            },
        ])
        .unwrap();
    let tip = repo
        .put_commit(
            branch.to_string(),
            Some(base.clone()),
            None,
            manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    repo.set_branch_head(branch, &tip).unwrap();
    repo.set_head(&tip).unwrap();
    let (server, state) = hosted(headless_capabilities(), Some(&base)).await;
    publish_base(&state, branch, &base);

    push(&repo, temp.path(), &server, branch)
        .await
        .expect("65 MiB of new content must publish without staged_v1");

    let hosted = state.lock().unwrap();
    let stored = hosted
        .chunked_blobs
        .get(large_hash.as_str())
        .expect("the large blob is recorded by its chunk mapping");
    assert_eq!(stored["size"].as_u64(), Some(65 * 1024 * 1024));
    assert!(
        stored["content"].as_array().unwrap().is_empty(),
        "no inline bytes"
    );
    assert!(stored["chunks"].as_array().unwrap().len() > 1);
    assert!(hosted.blobs.contains_key(small_hash.as_str()));
    assert_eq!(hosted.head_moves, 1);
    let (publication, preloads) = hosted.pushes.split_last().unwrap();
    assert_eq!((publication.commits, publication.blobs), (1, 0));
    assert!(preloads
        .iter()
        .all(|push| !push.had_branch && push.commits == 0));
    assert_eq!(preloads.iter().map(|push| push.blobs).sum::<usize>(), 2);
}

/// One commit on `branch` (after an empty base commit) adding `files`.
fn seed_commit_with_files(
    repo: &SqliteRepository,
    branch: &str,
    files: Vec<(String, Vec<u8>)>,
) -> (Hash, Hash) {
    repo.store_branch(&Branch::new(
        branch.to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch(branch).unwrap();
    let base = repo
        .put_commit(
            branch.to_string(),
            None,
            None,
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    let entries = files
        .into_iter()
        .map(|(path, bytes)| ManifestEntry {
            path,
            blob_hash: repo.put_blob(bytes).unwrap(),
            mode: FileMode::Regular,
        })
        .collect();
    let manifest = repo.put_manifest(entries).unwrap();
    let tip = repo
        .put_commit(
            branch.to_string(),
            Some(base.clone()),
            None,
            manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            Vec::new(),
        )
        .unwrap();
    repo.set_branch_head(branch, &tip).unwrap();
    repo.set_head(&tip).unwrap();
    (base, tip)
}

/// fb-632: a blob's chunk mapping is a pure function of its bytes. The same
/// large file must reach the server as the same chunk references whether the
/// push that carries it is an ordinary publication, a headless blob preload
/// or a phase-one bootstrap — otherwise an edit pushed by another transport
/// shares no chunk with the stored version and re-uploads the whole file.
#[tokio::test(flavor = "current_thread")]
async fn a_large_file_has_one_chunk_mapping_on_every_ordinary_wire_transport() {
    let mut state = 0x2545_F491u32;
    let large: Vec<u8> = (0..oak_core::LARGE_FILE_THRESHOLD as usize + 2_345_678)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xFF) as u8
        })
        .collect();
    let large_hash = oak_core::hash_bytes(&large);
    let expected: Vec<Value> = oak_core::blob_chunk_mapping(large.len() as u64, large.as_slice())
        .map(|chunk| {
            let (info, _) = chunk.unwrap();
            json!({"hash": info.hash.as_str(), "offset": info.offset, "size": info.length})
        })
        .collect();
    assert!(expected.len() > 1);
    let crowd = |count: usize| -> Vec<(String, Vec<u8>)> {
        (0..count)
            .map(|index| {
                (
                    format!("small/f{index:04}.txt"),
                    format!("small neighbour {index}\n").into_bytes(),
                )
            })
            .collect()
    };

    let mut seen = Vec::new();
    for transport in ["ordinary", "headless preload", "phase-one bootstrap"] {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let branch = "feature-one-mapping";
        let mut files = vec![("assets/large.bin".to_string(), large.clone())];
        if transport != "ordinary" {
            // 130 new blobs exceed the ordinary 128-blob envelope.
            files.extend(crowd(129));
        }
        let (base, _tip) = seed_commit_with_files(&repo, branch, files);
        let (server, state) = if transport == "phase-one bootstrap" {
            // A brand-new repository: nothing published, no repository head.
            hosted(headless_capabilities(), None).await
        } else {
            let (server, state) = hosted(headless_capabilities(), Some(&base)).await;
            publish_base(&state, branch, &base);
            (server, state)
        };

        push(&repo, temp.path(), &server, branch)
            .await
            .unwrap_or_else(|error| panic!("{transport}: {error}"));

        // Read the mapping off the wire: every `/push` request body that
        // carries the large blob.
        let pushes: Vec<Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == format!("/api/{OWNER}/{REPO}/push"))
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        let object_requests = pushes
            .iter()
            .filter(|push| push["commits"].as_array().is_none_or(Vec::is_empty))
            .count();
        assert_eq!(
            object_requests > 0,
            transport != "ordinary",
            "{transport}: {} push request(s)",
            pushes.len()
        );
        if transport == "headless preload" {
            assert!(pushes[..object_requests]
                .iter()
                .all(|push| push["branch"].is_null()));
        }
        let carried: Vec<&Value> = pushes
            .iter()
            .flat_map(|push| push["blobs"].as_array().unwrap())
            .filter(|blob| blob["hash"] == large_hash.as_str())
            .collect();
        assert_eq!(carried.len(), 1, "{transport}: the large blob is sent once");
        let stored = carried[0];
        assert!(
            stored["content"].as_array().unwrap().is_empty(),
            "{transport}: no inline bytes"
        );
        let chunks = stored["chunks"].as_array().unwrap().clone();
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk["hash"] != large_hash.as_str()),
            "{transport}: never one whole-blob chunk"
        );
        drop(state);
        seen.push((transport, chunks));
    }
    for (transport, chunks) in &seen {
        assert_eq!(chunks, &expected, "{transport} must send the one mapping");
    }
}

// ---------------------------------------------------------------------------
// The real binary: exit codes, JSON envelopes, `--plan`, and `oak watch`.
// ---------------------------------------------------------------------------

struct Checkout {
    temp: TempDir,
    base: Hash,
    branch: String,
}

impl Checkout {
    fn dir(&self) -> &Path {
        self.temp.path()
    }

    fn oak(&self, args: &[&str]) -> Output {
        self.oak_env(args, &[])
    }

    fn oak_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(self.dir())
            .env("HOME", self.dir().join(".home"))
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env("OAK_PROGRESS", "never")
            .env("NO_COLOR", "1")
            .env("OAK_WATCH_POLL_SECS", "1")
            .env_remove("OAK_API_KEY")
            .env_remove("OAK_REMOTE")
            .env_remove("OAK_REPO")
            .env_remove("OAK_WATCH")
            .env_remove("OAK_WATCH_AUTO_PULL")
            .env_remove("OAK_WATCH_AUTO_PUSH")
            .env_remove("OAK_WATCH_BLOCKED_PUSH_SECS")
            .envs(envs.iter().copied())
            .output()
            .expect("oak binary should run")
    }

    async fn run(self: &Arc<Self>, args: &'static [&'static str]) -> (i32, Value, String) {
        let checkout = self.clone();
        let out = tokio::task::spawn_blocking(move || checkout.oak(args))
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
            panic!("{args:?} did not print one JSON document ({error}): {stdout}\n{stderr}")
        });
        (out.status.code().unwrap_or(-1), value, stderr)
    }

    fn write_files(&self, dir: &str, count: usize) {
        std::fs::create_dir_all(self.dir().join(dir)).unwrap();
        for index in 0..count {
            std::fs::write(
                self.dir().join(dir).join(format!("f{index:04}.txt")),
                format!("{dir} file {index}\n"),
            )
            .unwrap();
        }
    }
}

/// A linked checkout whose first commit the server already has as both
/// `main` and the branch head.
fn linked_checkout(server: &MockServer, state: &Shared) -> Arc<Checkout> {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join(".home")).unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("README.md"), "base\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let base = repo.get_branch_head(&branch).unwrap().unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, OWNER).unwrap();
    repo.set_metadata(MetadataKey::RepoName, REPO).unwrap();
    drop(repo);
    {
        // The server holds everything the published base commit reaches.
        let mut hosted = state.lock().unwrap();
        hosted.repo_head = Some(base.to_string());
        let readme = b"base\n".to_vec();
        hosted
            .blobs
            .insert(oak_core::hash_bytes(&readme).to_string(), readme);
    }
    publish_base(state, &branch, &base);
    Arc::new(Checkout { temp, base, branch })
}

async fn mutating_requests(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request.method.as_str() != "GET"
                && !request.url.path().ends_with("/blobs/check")
                && !request.url.path().ends_with("/merge-preview")
        })
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_push_and_plan_agree_on_the_preload_and_publish_129_files() {
    let (server, state) = hosted(headless_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();

    let (code, plan, stderr) = checkout.run(&["push", "--plan", "--json"]).await;
    assert_eq!(code, 0, "{plan} {stderr}");
    assert_eq!(plan["plan_complete"], true);
    assert_eq!(plan["mutated"], false);
    assert_eq!(plan["transport"], "ordinary_blob_preload");
    assert_eq!(plan["preload_requests"], 2);
    assert_eq!(plan["blobs_missing_on_server_count"], 129);
    assert!(plan.get("publication_blocked").is_none());
    assert_eq!(
        plan["recommended_next_commands"],
        json!(["oak push --json"])
    );
    assert_eq!(mutating_requests(&server).await, Vec::<String>::new());

    let (code, receipt, stderr) = checkout.run(&["push", "--json"]).await;
    assert_eq!(code, 0, "{receipt} {stderr}");
    assert_eq!(receipt["published"], true);
    // The receipt states what was transferred, from server acknowledgements:
    // each of the 129 new files is its own single chunk, carried inline by
    // an acknowledged preload request.
    let bytes: usize = (0..129)
        .map(|index| format!("many file {index}\n").len())
        .sum();
    assert_eq!(receipt["chunks_required"], 129, "{receipt}");
    assert_eq!(receipt["chunks_already_present"], 0, "{receipt}");
    assert_eq!(receipt["chunks_uploaded"], 129, "{receipt}");
    assert_eq!(receipt["bytes_uploaded"], bytes, "{receipt}");
    assert!(
        !stderr.contains("Uploaded") && !stderr.contains("Preloaded"),
        "--json prints no progress: {stderr}"
    );
    let hosted = state.lock().unwrap();
    assert_eq!(
        receipt["pushed_head"],
        json!(hosted.branch_heads[&checkout.branch])
    );
    assert_ne!(
        hosted.branch_heads[&checkout.branch],
        checkout.base.to_string()
    );
    assert_eq!(hosted.preload_requests, 2);
    assert_eq!(hosted.head_moves, 1);
    assert!(hosted
        .pushes
        .iter()
        .all(|push| !push.branch_state_changed_before));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_refusal_is_exit_6_with_a_stable_code_and_a_truthful_plan() {
    let (server, state) = hosted(legacy_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();

    let (code, envelope, stderr) = checkout.run(&["push", "--json"]).await;
    assert_eq!(
        code, 6,
        "a server capability gap is not a usage error: {stderr}"
    );
    assert_eq!(envelope["error"]["code"], "push_capability_unavailable");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("129 new blobs > 128"), "{message}");
    assert!(message.contains("no remote state was mutated"), "{message}");
    assert_eq!(
        envelope["error"]["recommended_next_commands"],
        json!(["oak push --plan --json"])
    );

    // The recommended command works verbatim, stays read-only, and no
    // longer recommends the push that would be refused.
    let (code, plan, stderr) = checkout.run(&["push", "--plan", "--json"]).await;
    assert_eq!(code, 0, "{plan} {stderr}");
    assert_eq!(plan["plan_complete"], true);
    assert_eq!(plan["transport"], "unavailable");
    assert_eq!(
        plan["publication_blocked"]["code"],
        "push_capability_unavailable"
    );
    assert_eq!(plan["publication_blocked"]["message"], json!(message));
    assert_eq!(plan["recommended_next_commands"], json!([]));

    // `oak commit --push` reports the same refusal without retry advice.
    std::fs::write(checkout.dir().join("one-more.txt"), "x\n").unwrap();
    let (code, envelope, stderr) = checkout.run(&["commit", "--push", "--json"]).await;
    assert_eq!(code, 6, "{envelope} {stderr}");
    assert_eq!(envelope["error"]["code"], "commit_phase_failed");
    assert_eq!(envelope["error"]["commit"]["committed"], true);
    assert_eq!(envelope["error"]["commit"]["published"], false);
    assert!(envelope["error"]["commit"]["retry_command"].is_null());
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("cannot be published yet"), "{message}");
    assert!(!message.contains("Retry with"), "{message}");

    assert_eq!(mutating_requests(&server).await, Vec::<String>::new());
    assert_eq!(state.lock().unwrap().head_moves, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_preload_403_is_exit_6_capability_refusal_not_auth_denied() {
    let (server, state) = hosted(headless_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();
    state.lock().unwrap().preload_refusal_status = Some(403);

    let (code, envelope, stderr) = checkout.run(&["push", "--json"]).await;

    assert_eq!(code, 6, "{envelope} {stderr}");
    assert_eq!(envelope["error"]["code"], "push_capability_unavailable");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("the server refused blob preload for this repository"),
        "{message}"
    );
    assert!(message.contains("no branch state was changed"), "{message}");
    assert_eq!(
        envelope["error"]["recommended_next_commands"],
        json!(["oak status --json"])
    );
    assert_eq!(state.lock().unwrap().head_moves, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_push_plan_reports_the_ordinary_transport_without_a_capability_probe() {
    let (server, state) = hosted(legacy_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("few", 3);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();

    let (code, plan, stderr) = checkout.run(&["push", "--plan", "--json"]).await;

    assert_eq!(code, 0, "{plan} {stderr}");
    assert_eq!(plan["transport"], "ordinary");
    assert!(plan.get("preload_requests").is_none());
    assert_eq!(
        plan["recommended_next_commands"],
        json!(["oak push --json"])
    );
    assert_eq!(state.lock().unwrap().capability_probes, 0);
    assert_eq!(mutating_requests(&server).await, Vec::<String>::new());
}

async fn wait_for(what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct StopWatch(Arc<Checkout>);

impl Drop for StopWatch {
    fn drop(&mut self) {
        let _ = self.0.oak(&["watch", "stop", "--json"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_parks_a_capability_refusal_until_the_head_changes() {
    let (server, state) = hosted(legacy_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();
    let blocked_head = SqliteRepository::open(&checkout.dir().join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(&checkout.branch)
        .unwrap()
        .unwrap()
        .to_string();

    let _stop = StopWatch(checkout.clone());
    let (code, started, stderr) = checkout.run(&["watch", "start", "--json"]).await;
    assert_eq!(code, 0, "{started} {stderr}");

    let status = |checkout: &Checkout| -> Value {
        let out = checkout.oak(&["watch", "status", "--json"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    };
    let probe = checkout.clone();
    wait_for("watch to report the blocked push", 40, move || {
        !status(&probe)["push_blocked"].is_null()
    })
    .await;
    let report = status(&checkout);
    assert_eq!(report["running"], true);
    assert_eq!(
        report["push_blocked"]["code"],
        "push_capability_unavailable"
    );
    assert_eq!(report["push_blocked"]["head"], json!(blocked_head));
    assert!(report["last_push_error"]
        .as_str()
        .unwrap()
        .contains("129 new blobs > 128"));
    let (_, agent_state, _) = checkout.run(&["agent", "state", "--json"]).await;
    assert!(
        agent_state
            .to_string()
            .contains("\"push_blocked\":\"push_capability_unavailable\""),
        "{agent_state}"
    );

    // The old daemon retried after 10 s, then 20 s, ... forever. Parked, it
    // sends nothing more for this head.
    let probes_when_blocked = state.lock().unwrap().capability_probes;
    assert_eq!(probes_when_blocked, 1);
    tokio::time::sleep(Duration::from_secs(13)).await;
    assert_eq!(
        state.lock().unwrap().capability_probes,
        probes_when_blocked,
        "a deterministic refusal must not be retried while nothing changed"
    );

    // A new commit is something relevant changing: one new attempt.
    std::fs::write(checkout.dir().join("another.txt"), "x\n").unwrap();
    oak_cli::commands::commit::run(checkout.dir()).unwrap();
    let shared = state.clone();
    wait_for("watch to retry after the new commit", 40, move || {
        shared.lock().unwrap().capability_probes > probes_when_blocked
    })
    .await;
    let probe = checkout.clone();
    let head = blocked_head.clone();
    wait_for("watch to park the new head", 40, move || {
        status(&probe)["push_blocked"]["head"]
            .as_str()
            .is_some_and(|parked| parked != head)
    })
    .await;
    assert_eq!(mutating_watch_pushes(&server).await, 0);

    // The server gains the bridge and the agent pushes by hand: the parked
    // state must not outlive the publication.
    state.lock().unwrap().capabilities = headless_capabilities();
    let (code, receipt, stderr) = checkout.run(&["push", "--json"]).await;
    assert_eq!(code, 0, "{receipt} {stderr}");
    let probe = checkout.clone();
    wait_for("watch to clear the parked push", 40, move || {
        let report = status(&probe);
        report["running"] == true
            && report["push_blocked"].is_null()
            && report["last_push_error"].is_null()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_backs_off_normally_on_a_transient_preload_conflict_and_never_parks_it() {
    let (server, state) = hosted(headless_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();
    state.lock().unwrap().preload_refusal_status = Some(409);

    let _stop = StopWatch(checkout.clone());
    let (code, started, stderr) = checkout.run(&["watch", "start", "--json"]).await;
    assert_eq!(code, 0, "{started} {stderr}");
    let status = |checkout: &Checkout| -> Value {
        let out = checkout.oak(&["watch", "status", "--json"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    };
    let probe = checkout.clone();
    wait_for("watch to report the failed push", 40, move || {
        status(&probe)["last_push_error"].is_string()
    })
    .await;
    let report = status(&checkout);
    assert!(
        report["push_blocked"].is_null(),
        "a 409 is not parked: {report}"
    );
    assert!(
        report["last_push_error"]
            .as_str()
            .unwrap()
            .contains("HTTP 409"),
        "{report}"
    );

    // Ordinary backoff (10 s floor) brings the retry with no new commit.
    state.lock().unwrap().preload_refusal_status = None;
    let probe = checkout.clone();
    wait_for("the backoff retry to publish", 60, move || {
        let report = status(&probe);
        report["last_pushed_head"].is_string() && report["last_push_error"].is_null()
    })
    .await;
    assert!(status(&checkout)["push_blocked"].is_null());
    assert_eq!(state.lock().unwrap().head_moves, 1);
}

/// A push that must first bootstrap `main` is never a preload-then-publish,
/// and its plan must not say otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_for_a_main_bootstrap_never_reports_the_blob_preload() {
    let (server, state) = hosted(headless_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();
    {
        // Local `main` has history the server lacks: the repository exists
        // but has no `main`, and this branch is not published either.
        let repo = SqliteRepository::open(&checkout.dir().join(".oak/oak.db")).unwrap();
        if repo.get_branch("main").unwrap().is_none() {
            repo.store_branch(&Branch::new("main".to_string(), None, None))
                .unwrap();
        }
        let imported = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                oak_core::Tree::empty_hash(),
                "tester".to_string(),
                Some("imported".to_string()),
                Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &imported).unwrap();
        let mut hosted = state.lock().unwrap();
        hosted.repo_head = None;
        hosted.branch_heads.clear();
        hosted.branch_rows.clear();
    }

    let (code, plan, stderr) = checkout.run(&["push", "--plan", "--json"]).await;

    assert_eq!(code, 0, "{plan} {stderr}");
    assert_eq!(plan["plan_complete"], false);
    assert_eq!(plan["incomplete"][0]["reason"], "bootstrap_main");
    assert!(
        plan.get("transport").is_none() && plan.get("preload_requests").is_none(),
        "an undetermined plan names no transport: {plan}"
    );
    assert_eq!(mutating_requests(&server).await, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_rechecks_a_parked_push_on_the_timed_cadence_and_publishes_when_able() {
    let (server, state) = hosted(legacy_capabilities(), None).await;
    let checkout = linked_checkout(&server, &state);
    checkout.write_files("many", 129);
    oak_cli::commands::commit::run(checkout.dir()).unwrap();

    let _stop = StopWatch(checkout.clone());
    let starter = checkout.clone();
    let started = tokio::task::spawn_blocking(move || {
        starter.oak_env(
            &["watch", "start", "--json"],
            &[("OAK_WATCH_BLOCKED_PUSH_SECS", "3")],
        )
    })
    .await
    .unwrap();
    assert!(started.status.success(), "{started:?}");

    let status = |checkout: &Checkout| -> Value {
        let out = checkout.oak(&["watch", "status", "--json"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    };
    let probe = checkout.clone();
    wait_for("watch to report the blocked push", 40, move || {
        !status(&probe)["push_blocked"].is_null()
    })
    .await;
    let parked = status(&checkout)["push_blocked"].clone();
    assert_eq!(
        parked["next_retry_at"].as_u64().unwrap() - parked["since"].as_u64().unwrap(),
        3,
        "the override sets the recheck cadence: {parked}"
    );

    // No new commit: only the clock brings the next attempts.
    let shared = state.clone();
    wait_for("two timed rechecks of the same head", 60, move || {
        shared.lock().unwrap().capability_probes >= 3
    })
    .await;
    let again = status(&checkout)["push_blocked"].clone();
    assert_eq!(again["head"], parked["head"]);
    assert_eq!(
        again["since"], parked["since"],
        "the same blockage, not a new one"
    );
    assert_eq!(mutating_watch_pushes(&server).await, 0);

    // Once the server can take it, a recheck publishes and clears the state.
    state.lock().unwrap().capabilities = headless_capabilities();
    let probe = checkout.clone();
    wait_for("the recheck to publish", 60, move || {
        let report = status(&probe);
        report["push_blocked"].is_null() && report["last_pushed_head"] == parked["head"]
    })
    .await;
    assert_eq!(state.lock().unwrap().head_moves, 1);
}

async fn mutating_watch_pushes(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path().ends_with("/push")
        })
        .count()
}

// ---------------------------------------------------------------------------
// The concurrent preload: fan-out bound, admission probe, failure handling.
// (From the independent review of the transfer branch.)
// ---------------------------------------------------------------------------

fn preload_overlap(arrivals: &[Instant], delay: Duration) -> (usize, usize) {
    // (requests overlapping the first one, max requests in flight)
    let first = arrivals
        .iter()
        .skip(1)
        .filter(|at| **at < arrivals[0] + delay)
        .count();
    let max = arrivals
        .iter()
        .map(|at| {
            arrivals
                .iter()
                .filter(|other| **other <= *at && *at < **other + delay)
                .count()
        })
        .max()
        .unwrap_or(0);
    (first, max)
}

#[tokio::test(flavor = "current_thread")]
async fn preload_probe_runs_alone_and_fanout_is_bounded_to_three() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-fanout";
    let seed = seed_branch(&repo, branch, &[1000]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().push_delay_ms = 200;
    push(&repo, temp.path(), &server, branch).await.unwrap();
    let hosted = state.lock().unwrap();
    let (with_first, max) = preload_overlap(&hosted.preload_arrivals, Duration::from_millis(200));
    assert_eq!(with_first, 0, "the probe must run alone");
    assert!(max <= 3, "max in flight {max}");
    assert!(max >= 2, "no concurrency observed");
    drop(hosted);
    assert_published_once(&state, branch, &seed, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn transient_statuses_on_concurrent_preload_requests_are_retried() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-transient";
    let seed = seed_branch(&repo, branch, &[1000]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    {
        let mut hosted = state.lock().unwrap();
        hosted.push_delay_ms = 50;
        hosted.scripted_preload_status =
            HashMap::from([(2, 409), (3, 423), (4, 425), (6, 429), (7, 503)]);
    }
    push(&repo, temp.path(), &server, branch).await.unwrap();
    assert_published_once(&state, branch, &seed, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_423_after_the_probe_fails_closed_with_a_truthful_count() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-423";
    let seed = seed_branch(&repo, branch, &[1000]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    {
        let mut hosted = state.lock().unwrap();
        hosted.push_delay_ms = 20;
        hosted.preload_status_from = Some((3, 423));
    }
    let error = push(&repo, temp.path(), &server, branch).await.unwrap_err();
    let message = error.to_string();
    let hosted = state.lock().unwrap();
    assert_eq!(hosted.head_moves, 0);
    assert!(!hosted.branch_state_changed());
    let reported: usize = message
        .split(" of 1000 new blob(s) are stored")
        .next()
        .and_then(|head| head.rsplit(' ').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no stored count in: {message}"));
    assert_eq!(hosted.blobs.len(), reported, "{message}");
    assert!(hosted.pushes.iter().all(|push| !push.had_branch));
}

#[tokio::test(flavor = "current_thread")]
async fn a_401_midway_starts_no_new_preload_request_and_publishes_nothing() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-401";
    let seed = seed_branch(&repo, branch, &[1000]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    {
        let mut hosted = state.lock().unwrap();
        // The probe runs alone, then three requests fan out together; the
        // third arrival is refused at once while the other two take 300 ms.
        hosted.push_delay_ms = 300;
        hosted.scripted_preload_status = HashMap::from([(3, 401)]);
        hosted.scripted_status_immediate = true;
    }
    let error = push(&repo, temp.path(), &server, branch).await.unwrap_err();
    assert!(matches!(error, OakError::AuthRequired { .. }), "{error:?}");
    {
        let mut hosted = state.lock().unwrap();
        assert_eq!(hosted.head_moves, 0);
        assert!(!hosted.branch_state_changed());
        // The probe plus the three-wide fan-out: no request starts once the
        // 401 is observed, though the two siblings in flight may finish.
        assert_eq!(
            hosted.preload_arrivals.len(),
            4,
            "a request started after the 401"
        );
        assert!(hosted.pushes.iter().all(|push| push.commits == 0));
        hosted.pushes.clear();
    }
    push(&repo, temp.path(), &server, branch).await.unwrap();
    let hosted = state.lock().unwrap();
    for (hash, bytes) in &seed.files {
        assert_eq!(hosted.blobs.get(hash.as_str()), Some(bytes));
    }
    assert_eq!(hosted.head_moves, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn head_moving_during_concurrent_preload_is_followed() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch = "feature-headmove";
    let seed = seed_branch(&repo, branch, &[1000]);
    let (server, state) = hosted(headless_capabilities(), Some(&seed.base)).await;
    publish_base(&state, branch, &seed.base);
    state.lock().unwrap().push_delay_ms = 80;
    // `main` moves while the fan-out is in flight.
    let mover = state.clone();
    let moved = oak_core::hash_bytes(b"moved main").to_string();
    let moved_for_task = moved.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        mover.lock().unwrap().repo_head = Some(moved_for_task);
    });
    let result = push(&repo, temp.path(), &server, branch).await;
    task.await.unwrap();
    result.unwrap();
    assert_published_once(&state, branch, &seed, 1);
}
