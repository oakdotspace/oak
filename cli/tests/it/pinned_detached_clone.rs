//! `oak clone --detached --expected-head FULL` (fb-663 / fb-483): a pinned
//! read-only acquisition of the default branch. The wire selection is `main`;
//! the local checkout stays detached with no working branch.

use oak_core::protocol::tree_to_wire;
use oak_core::{
    hash_bytes, Commit, FileMode, Repository, SqliteRepository, Tree, TreeEntry, TreeEntryKind,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONTENT: &[u8] = b"pinned detached bytes";

struct Fixture {
    main: Commit,
    tree: Tree,
    blob: oak_core::Hash,
}

impl Fixture {
    fn new() -> Self {
        let blob = hash_bytes(CONTENT);
        let tree = Tree::new(vec![TreeEntry {
            name: "file.txt".to_string(),
            kind: TreeEntryKind::Blob,
            hash: blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        let main = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        )
        .unwrap();
        Self { main, tree, blob }
    }

    fn head(&self) -> &str {
        self.main.hash.as_str()
    }

    fn commit_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "hash": self.main.hash,
            "branch_name": self.main.branch_name,
            "parent_hash": self.main.parent_hash,
            "merge_parent_hash": self.main.merge_parent_hash,
            "manifest_hash": self.main.manifest_hash,
            "author": self.main.author,
            "message": self.main.message,
            "timestamp": self.main.timestamp.to_rfc3339(),
            "files": []
        })
    }

    fn branch_wire() -> serde_json::Value {
        serde_json::json!({
            "name": "main", "description": null, "parent_branch": null,
            "status": "open", "created_at": "2026-01-01T00:00:00Z"
        })
    }

    fn pull_body(&self) -> serde_json::Value {
        serde_json::json!({
            "head": self.main.hash,
            "branch": Self::branch_wire(),
            "branches": [Self::branch_wire()],
            "commits": [self.commit_wire()],
            "blobs": [{
                "hash": self.blob, "content": [], "size": CONTENT.len(),
                "chunks": [{"hash": self.blob, "offset": 0, "size": CONTENT.len()}]
            }],
            "trees": [tree_to_wire(&self.tree)],
            "renames": [],
            "restricted_blobs": [],
            "missing_content": []
        })
    }

    fn proof_body(&self) -> serde_json::Value {
        self.proof_body_at(None)
    }

    fn proof_body_at(&self, depth: Option<u32>) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "snapshot_token": "pinned-main-snapshot",
            "selected_branch": {"name": "main", "head": self.main.hash, "status": "open"},
            "scope": {"depth": depth, "commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
            "findings": []
        })
    }
}

async fn mount_exact_capability(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "schema_version": 1,
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1",
            "credential_presented": false,
            "credential_accepted": false
        })))
        .mount(server)
        .await;
}

async fn mount_chunks(server: &MockServer, fixture: &Fixture) {
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{"hash": fixture.blob, "content": CONTENT}]
        })))
        .mount(server)
        .await;
}

async fn run_oak(
    home: &std::path::Path,
    cwd: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("OAK_API_KEY")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .await
        .expect("run isolated oak")
}

async fn clone_pinned(
    server: &MockServer,
    home: &std::path::Path,
    destination: &std::path::Path,
    pin: &str,
    extra: &[&str],
) -> std::process::Output {
    let uri = server.uri();
    let dest = destination.to_str().unwrap();
    let mut args = vec![
        "clone",
        "--json",
        "--detached",
        "--expected-head",
        pin,
        "--remote",
        &uri,
    ];
    args.extend_from_slice(extra);
    args.extend(["oak/repo", dest]);
    run_oak(home, home, &args).await
}

fn stderr(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
}

async fn requests_to(server: &MockServer, request_path: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == request_path)
        .collect()
}

fn query_of(request: &wiremock::Request) -> std::collections::HashMap<String, String> {
    request.url.query_pairs().into_owned().collect()
}

/// A fully mocked pinned-main server that records every request.
async fn healthy_server(fixture: &Fixture) -> MockServer {
    healthy_server_at(fixture, None).await
}

async fn healthy_server_at(fixture: &Fixture, depth: Option<u32>) -> MockServer {
    let server = MockServer::start().await;
    mount_exact_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .and(query_param("selected_branch", "main"))
        .and(query_param("expected_head", fixture.head()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture.proof_body_at(depth)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .and(query_param("selected_branch", "main"))
        .and(query_param("expected_head", fixture.head()))
        .and(query_param("integrity_snapshot", "pinned-main-snapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture.pull_body()))
        .mount(&server)
        .await;
    mount_chunks(&server, fixture).await;
    server
}

#[tokio::test]
async fn full_pinned_detached_clone_lands_on_pin_without_a_branch() {
    let fixture = Fixture::new();
    let server = healthy_server(&fixture).await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");

    let output = clone_pinned(&server, home.path(), &destination, fixture.head(), &[]).await;
    assert!(output.status.success(), "{}", stderr(&output));

    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["result"]["detached"], true);
    assert_eq!(receipt["result"]["local_branch"], serde_json::Value::Null);
    assert_eq!(receipt["result"]["head"], fixture.head());
    assert_eq!(receipt["request"]["selected_branch"], "main");
    assert_eq!(receipt["request"]["expected_head"], fixture.head());
    assert_eq!(receipt["request"]["history"]["mode"], "full");
    assert_eq!(receipt["result"]["snapshot_bound"], true);
    assert_eq!(receipt["result"]["selected_branch_snapshot_bound"], true);
    assert_eq!(
        receipt["result"]["initial_pull_snapshot_bound"], true,
        "{receipt}"
    );

    assert_eq!(
        std::fs::read(destination.join("file.txt")).unwrap(),
        CONTENT
    );
    assert!(!destination.join(".oak/CLONE_IN_PROGRESS").exists());
    let repo = SqliteRepository::open(&destination.join(".oak/oak.db")).unwrap();
    assert_eq!(repo.get_current_branch_name().unwrap(), None);
    assert_eq!(repo.get_head().unwrap().as_ref(), Some(&fixture.main.hash));
    let branches: Vec<_> = repo
        .list_branches()
        .unwrap()
        .into_iter()
        .map(|branch| branch.name)
        .collect();
    assert_eq!(branches, vec!["main".to_string()], "no personal branch");
    drop(repo);

    let proofs = requests_to(&server, "/api/oak/repo/integrity").await;
    let pulls = requests_to(&server, "/api/oak/repo/pull").await;
    assert_eq!((proofs.len(), pulls.len()), (1, 1));
    for query in [query_of(&proofs[0]), query_of(&pulls[0])] {
        assert_eq!(query["selected_branch"], "main");
        assert_eq!(query["expected_head"], fixture.head());
        assert!(!query.contains_key("depth"), "{query:?}");
        assert!(!query.contains_key("branch") && !query.contains_key("branch_name"));
    }

    // Detached: committing is refused until `oak switch -c`.
    std::fs::write(destination.join("file.txt"), b"edited").unwrap();
    let commit = run_oak(home.path(), &destination, &["commit", "--json"]).await;
    assert_eq!(commit.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&commit.stdout).contains("oak switch -c"));
}

#[tokio::test]
async fn shallow_pinned_detached_clone_scopes_proof_and_pull_to_main() {
    let fixture = Fixture::new();
    let server = healthy_server_at(&fixture, Some(1)).await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");

    let output = clone_pinned(
        &server,
        home.path(),
        &destination,
        fixture.head(),
        &["--shallow"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["request"]["history"]["mode"], "shallow");
    assert_eq!(receipt["result"]["detached"], true);

    let proof = query_of(&requests_to(&server, "/api/oak/repo/integrity").await[0]);
    assert_eq!(proof["depth"], "1");
    assert_eq!(proof["branch"], "main");
    assert_eq!(proof["selected_branch"], "main");
    let pull = query_of(&requests_to(&server, "/api/oak/repo/pull").await[0]);
    assert_eq!(pull["depth"], "1");
    assert_eq!(pull["branch_name"], "main");
    assert_eq!(pull["selected_branch"], "main");
    assert_eq!(pull["expected_head"], fixture.head());
    assert_eq!(pull["integrity_snapshot"], "pinned-main-snapshot");
}

#[tokio::test]
async fn pinned_detached_clone_forwards_sparse_paths() {
    let fixture = Fixture::new();
    let server = healthy_server(&fixture).await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");

    let output = clone_pinned(
        &server,
        home.path(),
        &destination,
        fixture.head(),
        &["--path", "file.txt"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let proof = query_of(&requests_to(&server, "/api/oak/repo/integrity").await[0]);
    let pull = query_of(&requests_to(&server, "/api/oak/repo/pull").await[0]);
    assert_eq!(proof["paths"], "file.txt");
    assert_eq!(pull["paths"], "file.txt");
    assert_eq!(pull["selected_branch"], "main");
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["request"]["materialization"]["mode"], "sparse");
    assert_eq!(receipt["result"]["detached"], true);
}

#[tokio::test]
async fn stale_pin_is_refused_by_the_proof_before_any_pull_or_destination() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    mount_exact_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(
            ResponseTemplate::new(412)
                .set_body_string("selected branch 'main' does not match expected_head"),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");

    let output = clone_pinned(&server, home.path(), &destination, fixture.head(), &[]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("does not match expected_head"),
        "{}",
        stderr(&output)
    );
    assert!(!destination.exists());
    assert_eq!(
        requests_to(&server, "/api/oak/repo/integrity").await.len(),
        1
    );
    assert!(requests_to(&server, "/api/oak/repo/pull").await.is_empty());
}

#[tokio::test]
async fn head_moving_between_proof_and_pull_removes_the_guarded_destination() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    mount_exact_capability(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture.proof_body()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pull"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("repo");

    let output = clone_pinned(&server, home.path(), &destination, fixture.head(), &[]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("exact pinned clone was not acquired"),
        "{}",
        stderr(&output)
    );
    assert!(!destination.exists());
    assert_eq!(
        requests_to(&server, "/api/oak/repo/integrity").await.len(),
        1
    );
    assert_eq!(requests_to(&server, "/api/oak/repo/pull").await.len(), 1);
}

#[tokio::test]
async fn legacy_and_non_exact_servers_refuse_before_checkout() {
    let fixture = Fixture::new();
    for legacy in [true, false] {
        let server = MockServer::start().await;
        if legacy {
            Mock::given(method("GET"))
                .and(path("/api/oak/repo/integrity/capabilities"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/oak/repo"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "repo"})),
                )
                .mount(&server)
                .await;
        } else {
            Mock::given(method("GET"))
                .and(path("/api/oak/repo/integrity/capabilities"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "schema_version": 1,
                    "clone_preflight_profile": "bounded_v1",
                    "credential_presented": false,
                    "credential_accepted": false
                })))
                .mount(&server)
                .await;
        }
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("repo");
        let output = clone_pinned(
            &server,
            home.path(),
            &destination,
            fixture.head(),
            &["--allow-legacy-scope"],
        )
        .await;
        assert!(!output.status.success(), "legacy={legacy}");
        assert!(
            stderr(&output).contains("exact selected-branch acquisition"),
            "legacy={legacy}: {}",
            stderr(&output)
        );
        assert!(!destination.exists());
        assert!(requests_to(&server, "/api/oak/repo/pull").await.is_empty());
        assert!(requests_to(&server, "/api/oak/repo/integrity")
            .await
            .is_empty());
    }
}

/// The pull is not trusted just because it answered 200: main metadata, the
/// pinned commit, and its manifest must all be present or nothing is created.
#[tokio::test]
async fn incomplete_pinned_pull_fails_closed() {
    let fixture = Fixture::new();
    let wrong_commit = {
        let other = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            fixture.tree.hash.clone(),
            "tester".to_string(),
            Some("different".to_string()),
            Vec::new(),
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:05Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        )
        .unwrap();
        assert_ne!(other.hash, fixture.main.hash);
        other
    };
    type Mutation = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "missing main metadata",
            Box::new(|body| {
                body["branch"] = serde_json::Value::Null;
                body["branches"] = serde_json::json!([]);
            }),
            "omitted the snapshot-bound selected branch",
        ),
        (
            "missing pinned commit",
            Box::new(|body| body["commits"] = serde_json::json!([])),
            "omitted or duplicated",
        ),
        (
            "different commit than the pin",
            Box::new({
                let other = wrong_commit.clone();
                move |body| {
                    body["head"] = serde_json::json!(other.hash);
                    body["commits"] = serde_json::json!([{
                        "hash": other.hash,
                        "branch_name": "main",
                        "parent_hash": null,
                        "merge_parent_hash": null,
                        "manifest_hash": other.manifest_hash,
                        "author": other.author,
                        "message": other.message,
                        "timestamp": other.timestamp.to_rfc3339(),
                        "files": []
                    }]);
                }
            }),
            "omitted or duplicated",
        ),
        (
            "missing manifest",
            Box::new(|body| body["trees"] = serde_json::json!([])),
            "exact-head manifest",
        ),
    ];
    for (label, mutate, expected_error) in cases {
        let server = MockServer::start().await;
        mount_exact_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture.proof_body()))
            .mount(&server)
            .await;
        let mut body = fixture.pull_body();
        mutate(&mut body);
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        mount_chunks(&server, &fixture).await;
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("repo");
        let output = clone_pinned(&server, home.path(), &destination, fixture.head(), &[]).await;
        assert!(!output.status.success(), "{label} must fail closed");
        assert!(
            stderr(&output).contains(expected_error),
            "{label}: {}",
            stderr(&output)
        );
        assert!(!destination.exists(), "{label} left a destination");
    }
}

fn parse_args(args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .args(["--remote", "http://127.0.0.1:9", "o/r", "d"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap()
}

#[test]
fn parser_refuses_bad_pin_shapes_before_any_network_or_destination() {
    let full = "a".repeat(64);
    for args in [
        vec!["clone", "--detached", "--expected-head", "abc123"],
        vec!["clone", "--detached", "--expected-head", &"A".repeat(64)],
        vec!["clone", "--expected-head", &full],
        vec![
            "clone",
            "--detached",
            "--branch",
            "x",
            "--expected-head",
            &full,
        ],
        vec![
            "clone",
            "--detached",
            "--branch",
            "main",
            "--expected-head",
            &full,
        ],
        vec!["clone", "--branch", "main", "--expected-head", &full],
    ] {
        let output = parse_args(&args);
        assert!(!output.status.success(), "{args:?} should be refused");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !text.contains("error sending request") && !text.contains("Connection refused"),
            "{args:?} reached the network: {text}"
        );
    }
    let output = parse_args(&["clone", "--expected-head", &full]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("--branch NAME or --detached"));
}
