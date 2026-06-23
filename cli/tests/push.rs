use chrono::Utc;
use oak_cli::output;
use oak_core::{
    Branch, ChangeType, FileChange, FileMode, ManifestEntry, MetadataKey, OakError, Repository,
    SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn seed_one_commit(repo: &SqliteRepository, branch_name: &str) {
    let branch = Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    );
    repo.store_branch(&branch).unwrap();
    repo.set_current_branch(branch_name).unwrap();

    let blob_hash = repo.put_blob(b"hello world\n".to_vec()).unwrap();
    let manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "hello.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let commit_hash = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "hello.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &commit_hash).unwrap();
    repo.set_head(&commit_hash).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn push_repo_flag_links_and_creates_repo_non_interactively() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "hello agents\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch_name = repo.get_current_branch_name().unwrap().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos"))
        .and(body_json(serde_json::json!({
            "name": "widget",
            "description": null,
            "organization_slug": "agents"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widget"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::run(temp.path(), &server.uri(), false, Some("agents/widget"))
        .await
        .unwrap();
    let captured = output::end_capture();

    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected push result, got: {captured:?}"
    );
    assert!(
        captured.contains("Run `oak merge` to land this on main"),
        "expected headless next-step guidance after creating a remote repo, got: {captured:?}"
    );

    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_metadata(MetadataKey::RepoOwner)
            .unwrap()
            .as_deref(),
        Some("agents")
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::RepoName).unwrap().as_deref(),
        Some("widget")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_unlinked_noninteractive_is_local_configuration_error() {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "hello agents\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();

    let server = MockServer::start().await;

    output::begin_capture();
    let err = oak_cli::commands::push::run(temp.path(), &server.uri(), false, None)
        .await
        .unwrap_err();
    let captured = output::end_capture();

    let msg = err.to_string();
    assert!(
        matches!(err, OakError::Config(_)),
        "expected local configuration error, got: {msg}"
    );
    assert!(
        msg.contains("This repository isn't linked to a remote"),
        "error: {msg}"
    );
    assert!(msg.contains("oak push --repo <org>/<repo>"), "error: {msg}");
    assert!(
        !msg.contains("Server error"),
        "local setup error should not look like a server failure: {msg}"
    );
    assert!(
        captured.is_empty(),
        "unexpected captured output: {captured}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_output_includes_branch_url() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    // A tiny all-inline payload skips the blobs/check round trip entirely —
    // the dedup probe costs more latency than just sending the bytes.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    // Piped/captured push prints a single result line — the step-by-step
    // narration (Pushing/Checking/Uploaded/Push complete) is terminal-only.
    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected single piped result line, got: {captured:?}"
    );
    assert!(!captured.contains("Pushing"), "got: {captured:?}");
    assert!(!captured.contains("Push complete"), "got: {captured:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn push_encodes_branch_name_in_api_path() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "feature/foo bar";
    seed_one_commit(&repo, branch_name);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature%2Ffoo%20bar"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .unwrap();
    output::end_capture();
}

#[tokio::test(flavor = "current_thread")]
async fn push_success_without_branch_keeps_plain_output() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Tiny all-inline payload: the blobs/check dedup round trip is skipped
    // (sending the bytes is cheaper than the probe).
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let captured = output::end_capture();

    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "expected single piped result line, got: {captured:?}"
    );
    assert!(!captured.contains("/branches/"));
}

#[tokio::test(flavor = "current_thread")]
async fn push_remote_commit_conflict_returns_one_actionable_error_without_preprinting() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);

    let remote_head = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Fetched twice: once by the push's concurrent head GET, once by the
    // self-heal probe re-resolving the branch head.
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": remote_head
        })))
        .expect(2)
        .mount(&server)
        .await;
    // No /commits/info on this server: the self-heal can't establish that
    // re-parenting is safe and must fall back to the actionable error.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .expect(0)
        .mount(&server)
        .await;

    output::begin_capture();
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("push should reject a remote branch head missing locally");
    let captured = output::end_capture();

    assert!(
        captured.is_empty(),
        "push_async should return the user-facing error and leave printing to the CLI entry point, got: {captured:?}"
    );
    assert!(
        matches!(err, OakError::RemoteCommitsNotInLocalHistory),
        "expected a specific push conflict variant, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "The remote branch has commits this clone doesn't have.\n\
         Run 'oak pull' to bring them in and converge (your local commits are kept), then push again."
    );
}

/// The trap, resolved by `oak push` itself (Invariant 3): the server's
/// branch head is a moved seed — a `main` commit minted when main advanced
/// under the branch, with no real work on the remote branch. Push must
/// re-parent automatically, print exactly one info line, and complete in
/// the same invocation.
#[tokio::test(flavor = "current_thread")]
async fn push_self_heals_when_remote_head_is_a_moved_seed() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    // The seed's snapshot on the server: one file the branch doesn't have.
    let theirs_content = b"merged on main\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = {
        let scratch_dir = TempDir::new().unwrap();
        let scratch = SqliteRepository::open(&scratch_dir.path().join("scratch.db")).unwrap();
        let root = scratch
            .put_manifest(vec![ManifestEntry {
                path: "main.txt".to_string(),
                blob_hash: theirs_blob.clone(),
                mode: FileMode::Regular,
            }])
            .unwrap();
        let mut fetch = |h: &oak_core::Hash| -> oak_core::Result<oak_core::Tree> {
            scratch
                .get_tree(h)?
                .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
        };
        let trees = oak_core::collect_tree_objects(&root, &mut fetch).unwrap();
        let wire: Vec<serde_json::Value> = trees
            .iter()
            .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
            .collect();
        (root.to_string(), wire)
    };
    let seed: String = "a1".repeat(32);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed
        })))
        .mount(&server)
        .await;
    // The seed is a `main` commit: a seed that moved, not foreign work.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": seed,
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": root,
                "author": "<remote>",
                "message": "merged something else",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "files": []
            }],
            "trees": wire_trees
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "blobs": [{
                "hash": theirs_blob.to_string(),
                "content": [],
                "size": theirs_content.len(),
                "chunks": [{
                    "hash": theirs_blob.to_string(),
                    "offset": 0,
                    "size": theirs_content.len()
                }]
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/chunks/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chunks": [{
                "hash": theirs_blob.to_string(),
                "download_url": null,
                "content": theirs_content.to_vec()
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("push must self-heal a moved seed and complete");
    let captured = output::end_capture();

    // Exactly one re-parent info line, then the piped result line.
    let reparent_lines: Vec<&str> = captured
        .lines()
        .filter(|l| l.contains("re-parented onto main@"))
        .collect();
    assert_eq!(
        reparent_lines.len(),
        1,
        "expected exactly one re-parent line, got: {captured:?}"
    );
    assert!(
        reparent_lines[0].contains("main advanced since this branch was created"),
        "got: {captured:?}"
    );
    assert!(
        captured.contains(&format!("Pushed to {}", server.uri())),
        "push must complete in the same invocation, got: {captured:?}"
    );

    // The branch now extends the seed; the old tip stays reachable.
    let new_head = repo.get_branch_head(branch_name).unwrap().unwrap();
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed.clone())
    );
    assert_eq!(commit.merge_parent_hash, Some(old_tip.clone()));
    assert!(repo.get_commit(&old_tip).unwrap().is_some());

    // Overlay carries both sides.
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let mut paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["hello.txt", "main.txt"]);

    // Canonical main: the moved seed is recorded under the server's hash.
    assert_eq!(
        repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
        Some(seed)
    );
}

/// When the remote branch holds real foreign commits (its head is a commit
/// on the branch itself, not a moved seed), push must NOT touch anything —
/// one instruction: `oak pull`.
#[tokio::test(flavor = "current_thread")]
async fn push_does_not_self_heal_over_foreign_branch_commits() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    let foreign: String = "b2".repeat(32);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": foreign
        })))
        .mount(&server)
        .await;
    // The remote head is a commit on the branch itself — real foreign work.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": foreign,
                "branch_name": branch_name,
                "parent_hash": null,
                "manifest_hash": oak_core::Tree::empty_hash().to_string(),
                "author": "someone-else",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "files": []
            }],
            "trees": []
        })))
        .mount(&server)
        .await;

    output::begin_capture();
    let err = oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect_err("foreign remote commits must abort the push");
    output::end_capture();

    assert!(
        matches!(err, OakError::RemoteCommitsNotInLocalHistory),
        "expected the pull instruction, got: {err:?}"
    );
    // Nothing moved locally.
    assert_eq!(repo.get_branch_head(branch_name).unwrap(), Some(old_tip));
}

/// The deferred-heal retry: a prior attempt (e.g. the auto-push inside
/// `oak commit`, which holds the workdir lock) already ingested the moved
/// seed locally, so the seed IS in the local DB — but the branch still
/// doesn't extend it. The next push must detect divergence by ANCESTRY and
/// self-heal, not wave the stale chain through to a server-side rejection.
#[tokio::test(flavor = "current_thread")]
async fn push_self_heals_when_seed_is_already_ingested_but_not_ancestor() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let branch_name = "tester-a71b16";
    seed_one_commit(&repo, branch_name);
    let old_tip = repo.get_branch_head(branch_name).unwrap().unwrap();

    // The moved seed, already fully materialized locally (commit row +
    // empty tree) — exactly what a deferred self-heal leaves behind.
    let seed: String = "a1".repeat(32);
    repo.store_commit(&oak_core::Commit {
        hash: oak_core::Hash(seed.clone()),
        branch_name: "main".to_string(),
        parent_hash: None,
        merge_parent_hash: None,
        manifest_hash: oak_core::Tree::empty_hash(),
        author: "<remote>".to_string(),
        message: None,
        timestamp: Utc::now(),
        files: Vec::new(),
    })
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::push::push_async(
        &repo,
        temp.path(),
        &server.uri(),
        "oak",
        "oak",
        Some(branch_name),
        false,
        None,
    )
    .await
    .expect("retry after a deferred heal must self-heal and push");
    let captured = output::end_capture();

    assert!(
        captured.contains("re-parented onto main@"),
        "expected the re-parent line, got: {captured:?}"
    );
    let new_head = repo.get_branch_head(branch_name).unwrap().unwrap();
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed)
    );
    assert_eq!(commit.merge_parent_hash, Some(old_tip));
}
