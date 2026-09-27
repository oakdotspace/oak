use chrono::Utc;
use oak_cli::output;
use oak_core::{
    Branch, ChangeType, Commit, FileChange, FileMode, Hash, ManifestEntry, MetadataKey, OakError,
    Repository, SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "current_thread")]
async fn pull_blob_ingest_validates_mapping_before_any_local_write() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    let first = b"first".to_vec();
    let second = b"second".to_vec();
    let first_hash = oak_core::hash_bytes(&first);
    let second_hash = oak_core::hash_bytes(&second);
    let mut blob_content = first;
    blob_content.extend_from_slice(&second);
    let blob_hash = oak_core::hash_bytes(&blob_content);
    let blobs = vec![oak_core::protocol::BlobData {
        hash: blob_hash.to_string(),
        content: Vec::new(),
        size: blob_content.len() as u64,
        chunks: vec![
            oak_core::protocol::ChunkRefData {
                hash: first_hash.to_string(),
                offset: 1,
                size: 5,
            },
            oak_core::protocol::ChunkRefData {
                hash: second_hash.to_string(),
                offset: 6,
                size: 6,
            },
        ],
        mapping_proof_token: None,
    }];

    let error = oak_cli::commands::pull::fetch_and_store_blobs(
        &repo,
        &blobs,
        &oak_cli::http::api_client(),
        "http://127.0.0.1:1",
        "oak",
        "oak",
        None,
    )
    .await
    .expect_err("pull must reject the descriptor before attempting the network");

    assert!(error.to_string().contains("offset"), "got: {error}");
    assert!(!repo.has_blob(&blob_hash).unwrap());
    assert!(repo.get_blob_chunks(&blob_hash).unwrap().is_none());
    assert!(repo.get_chunk(&first_hash).unwrap().is_none());
    assert!(repo.get_chunk(&second_hash).unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn pull_409_returns_one_actionable_conflict_error_without_preprinting() {
    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&oak_dir).unwrap();
    let local_head =
        Hash::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    // No current branch is set in this repo, so the 409 can't be resolved by
    // re-parenting (that requires the checked-out branch) and must surface
    // as the conflict error.
    let err = oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&local_head),
        false,
        temp.path(),
        None,
    )
    .await
    .expect_err("unresolvable 409 pull response must be surfaced as a conflict");
    let captured = output::end_capture();

    assert!(
        captured.is_empty(),
        "pull_async should return the user-facing error and leave printing to the CLI entry point, got: {captured:?}"
    );
    assert!(
        matches!(err, OakError::LocalCommitsNotInRemoteHistory),
        "expected a specific pull conflict variant, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("Run 'oak pull' to converge"),
        "error must offer the converging pull, got: {msg}"
    );
    assert!(
        msg.contains("nothing is deleted"),
        "error must make the keepsafe explicit, got: {msg}"
    );
    assert!(
        !msg.contains("discard local commits"),
        "error must not present --force as destructive-only exit, got: {msg}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pull_skips_rename_when_target_exists_as_orphan_branch_head() {
    let temp = TempDir::new().unwrap();
    let oak_dir = temp.path().join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&oak_dir).unwrap();
    let manifest = repo.put_manifest(Vec::new()).unwrap();

    repo.store_branch(&Branch::new("old".to_string(), None, None))
        .unwrap();
    repo.set_current_branch("old").unwrap();

    let old_head = repo
        .put_commit(
            "old".to_string(),
            None,
            None,
            manifest.clone(),
            "tester".to_string(),
            Some("old".to_string()),
            Utc::now(),
            vec![],
        )
        .unwrap();
    let target_head = repo
        .put_commit(
            "target".to_string(),
            None,
            None,
            manifest,
            "tester".to_string(),
            Some("target".to_string()),
            Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("old", &old_head).unwrap();
    // This mirrors fresh clones of repos that have remote-only branch heads:
    // the target name is reserved in branch_heads even though no branch row
    // exists locally.
    repo.set_branch_head("target", &target_head).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": [],
            "renames": [{
                "id": 7,
                "old_name": "old",
                "new_name": "target",
                "renamed_at": Utc::now().to_rfc3339()
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("old"),
        Some(&old_head),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("rename replay should skip reserved target names");
    let captured = output::end_capture();

    assert!(
        captured.contains("Skipping rename 'old' -> 'target'"),
        "expected skipped-rename warning, got: {captured:?}"
    );
    assert!(captured.contains("Already up to date"), "got: {captured:?}");
    assert_eq!(repo.get_branch_head("old").unwrap(), Some(old_head));
    assert_eq!(repo.get_branch_head("target").unwrap(), Some(target_head));
    assert_eq!(
        repo.get_metadata(MetadataKey::LastRenameId)
            .unwrap()
            .as_deref(),
        Some("7")
    );
}

// ---------------------------------------------------------------------------
// Snapshot re-parenting (Invariant 2) + --force keepsafe
// ---------------------------------------------------------------------------

/// Build the wire-shaped tree objects (and root hash) for a server-side
/// snapshot, using a scratch repo so the hashes are the real content
/// addresses the client will verify against.
fn wire_tree_fixture(entries: Vec<ManifestEntry>) -> (String, Vec<serde_json::Value>) {
    let tmp = TempDir::new().unwrap();
    let scratch = SqliteRepository::open(&tmp.path().join("scratch.db")).unwrap();
    let root = scratch.put_manifest(entries).unwrap();
    let mut fetch = |h: &Hash| -> oak_core::Result<oak_core::Tree> {
        scratch
            .get_tree(h)?
            .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
    };
    let trees = oak_core::collect_tree_objects(&root, &mut fetch).unwrap();
    let wire = trees
        .iter()
        .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
        .collect();
    (root.to_string(), wire)
}

/// Workdir-shaped local repo on branch `tester` with one commit adding
/// `ours.txt`, the file materialized on disk. Returns (repo, old tip).
fn seed_diverged_workdir(temp: &TempDir, server_uri: &str) -> (SqliteRepository, Hash) {
    let oak_dir = temp.path().join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, server_uri)
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();

    repo.store_branch(&Branch::new(
        "tester".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("tester").unwrap();

    let ours_blob = repo.put_blob(b"ours\n".to_vec()).unwrap();
    let ours_manifest = repo
        .put_manifest(vec![ManifestEntry {
            path: "ours.txt".to_string(),
            blob_hash: ours_blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let old_tip = repo
        .put_commit(
            "tester".to_string(),
            None,
            None,
            ours_manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "ours.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(ours_blob),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head("tester", &old_tip).unwrap();
    repo.set_head(&old_tip).unwrap();
    std::fs::write(temp.path().join("ours.txt"), "ours\n").unwrap();
    (repo, old_tip)
}

/// Mount the wiremock choreography for a moved-seed divergence: the server
/// 409s the pull, reports `seed` as both main's head and the branch's head,
/// and serves the seed's commit metadata + trees + blob bytes.
async fn mount_moved_seed_server(
    server: &MockServer,
    seed: &Hash,
    root: &str,
    timestamp: chrono::DateTime<Utc>,
    wire_trees: &[serde_json::Value],
    theirs_path: &str,
    theirs_content: &[u8],
) {
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": seed.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": root,
                "author": "<remote>",
                "message": "merged something",
                "timestamp": timestamp.to_rfc3339(),
                "files": []
            }],
            "trees": wire_trees
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/oak/raw/{}/{theirs_path}",
            seed.as_str()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(theirs_content.to_vec()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(server)
        .await;
}

/// The trap, resolved by `oak pull`: branch created off a main the server
/// has since moved past (or recorded under a different identity). The 409
/// must converge via snapshot re-parenting — one new commit parented on the
/// server's head, old tip kept reachable, disjoint changes overlaid clean.
#[tokio::test(flavor = "current_thread")]
async fn pull_409_reparents_branch_onto_moved_seed() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    let theirs_content = b"theirs\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = wire_tree_fixture(vec![ManifestEntry {
        path: "theirs.txt".to_string(),
        blob_hash: theirs_blob,
        mode: FileMode::Regular,
    }]);
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_000_500, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        Hash(root.clone()),
        "<remote>".to_string(),
        Some("merged something".to_string()),
        Vec::new(),
        seed_timestamp,
    )
    .unwrap()
    .hash;
    mount_moved_seed_server(
        &server,
        &seed,
        &root,
        seed_timestamp,
        &wire_trees,
        "theirs.txt",
        theirs_content,
    )
    .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("diverged pull must converge by re-parenting");
    let captured = output::end_capture();

    assert!(
        captured.contains("Re-parented 'tester' onto main@"),
        "expected the re-parent line, got: {captured:?}"
    );

    let new_head = repo.get_branch_head("tester").unwrap().unwrap();
    assert_ne!(new_head, old_tip);
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed.to_string()),
        "re-parent commit must extend the server's branch head"
    );
    assert_eq!(
        commit.merge_parent_hash,
        Some(old_tip.clone()),
        "old tip must stay reachable via merge_parent_hash"
    );

    // Overlay: ours applied onto the seed snapshot.
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let mut paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["ours.txt", "theirs.txt"]);

    // Clean worktree was reset to the overlay — file bytes from both sides.
    assert_eq!(
        std::fs::read(temp.path().join("ours.txt")).unwrap(),
        b"ours\n"
    );
    assert_eq!(
        std::fs::read(temp.path().join("theirs.txt")).unwrap(),
        b"theirs\n"
    );

    // Nothing lost; local main canonicalized to the server's hash verbatim.
    assert!(repo.get_commit(&old_tip).unwrap().is_some());
    assert_eq!(
        repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
        Some(seed.to_string())
    );
}

/// Overlapping edits route into the EXISTING conflict flow (`--continue` /
/// `--abort`) — no new UX. The resolved commit re-seeds the branch on the
/// server's head with the old tip as merge parent.
#[tokio::test(flavor = "current_thread")]
async fn pull_409_overlapping_edits_route_to_existing_conflict_flow() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    // Server's snapshot also has ours.txt, with different content: overlap.
    let theirs_content = b"theirs version\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = wire_tree_fixture(vec![ManifestEntry {
        path: "ours.txt".to_string(),
        blob_hash: theirs_blob,
        mode: FileMode::Regular,
    }]);
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_000_600, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        Hash(root.clone()),
        "<remote>".to_string(),
        Some("merged something".to_string()),
        Vec::new(),
        seed_timestamp,
    )
    .unwrap()
    .hash;
    mount_moved_seed_server(
        &server,
        &seed,
        &root,
        seed_timestamp,
        &wire_trees,
        "ours.txt",
        theirs_content,
    )
    .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    let err = oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect_err("overlapping edits must pause in the conflict flow");
    let captured = output::end_capture();
    assert!(
        matches!(err, OakError::MergeConflict(1)),
        "expected MergeConflict, got: {err:?}"
    );
    assert!(
        captured.contains("oak pull --continue"),
        "must point at the existing conflict flow, got: {captured:?}"
    );

    // The pause is the regular sync pause: SYNC_HEAD + markered file.
    let sync_head = std::fs::read_to_string(temp.path().join(".oak/SYNC_HEAD")).unwrap();
    let lines: Vec<&str> = sync_head.lines().collect();
    assert_eq!(lines[0], "main");
    assert_eq!(lines[1], "tester");
    assert_eq!(lines[2], seed.as_str());
    assert_eq!(
        lines[3],
        old_tip.as_str(),
        "re-parent pause records the old tip"
    );
    let conflicted = std::fs::read_to_string(temp.path().join("ours.txt")).unwrap();
    assert!(
        conflicted.contains("ours") && conflicted.contains("theirs version"),
        "both sides must be in the conflict write-out: {conflicted}"
    );

    // Resolve and continue through the EXISTING flow.
    std::fs::write(temp.path().join("ours.txt"), "resolved\n").unwrap();
    drop(lock);
    output::begin_capture();
    oak_cli::commands::sync::sync_continue(temp.path()).unwrap();
    output::end_capture();

    let new_head = repo.get_branch_head("tester").unwrap().unwrap();
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(
        commit.parent_hash.as_ref().map(|h| h.to_string()),
        Some(seed.to_string()),
        "resolved commit must re-seed the branch on the server's head"
    );
    assert_eq!(commit.merge_parent_hash, Some(old_tip));
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == "ours.txt")
        .expect("resolved path committed");
    let blob = repo.get_blob(&entry.blob_hash).unwrap().unwrap();
    assert_eq!(blob.content, b"resolved\n");
}

/// `oak pull --json` (fb-319): a divergence re-parent that pauses on
/// overlapping edits still prints ONE receipt on stdout (convergence
/// `conflict`, the `--continue`/`--abort` commands, the conflicted path) and
/// keeps the historical exit code 5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_json_divergence_conflict_prints_receipt_and_exits_5() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());
    drop(repo);

    let theirs_content = b"theirs version\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = wire_tree_fixture(vec![ManifestEntry {
        path: "ours.txt".to_string(),
        blob_hash: theirs_blob,
        mode: FileMode::Regular,
    }]);
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_000_700, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        Hash(root.clone()),
        "<remote>".to_string(),
        Some("merged something".to_string()),
        Vec::new(),
        seed_timestamp,
    )
    .unwrap()
    .hash;
    mount_moved_seed_server(
        &server,
        &seed,
        &root,
        seed_timestamp,
        &wire_trees,
        "ours.txt",
        theirs_content,
    )
    .await;

    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(temp.path())
        .env("HOME", &home)
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .args(["pull", "--json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stdout={stdout} stderr={stderr}"
    );
    let mut docs = serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let json = docs.next().expect("one receipt").expect("valid JSON");
    assert!(docs.next().is_none(), "exactly one document: {stdout}");
    assert!(json.get("error").is_none(), "receipt, not envelope: {json}");
    assert_eq!(json["branch"], "tester");
    assert_eq!(json["head_before"], old_tip.as_str());
    assert_eq!(json["convergence"], "conflict");
    assert_eq!(json["parent_sync"], "not_run");
    assert_eq!(json["parent_synced"], false);
    assert_eq!(json["conflict_count"], 1);
    assert_eq!(json["conflict_paths"], serde_json::json!(["ours.txt"]));
    assert!(json["commits_fetched"].is_null());
    assert!(json["unknowns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field == "commits_fetched"));
    let next = json["recommended_next_commands"].as_array().unwrap();
    assert!(next.iter().any(|c| c == "oak pull --continue"), "{json}");
    assert!(next.iter().any(|c| c == "oak pull --abort"), "{json}");
    // Narration (the conflict instructions) went to stderr, not stdout.
    assert!(stderr.contains("oak pull --continue"), "stderr: {stderr}");
    assert!(temp.path().join(".oak/SYNC_HEAD").exists());
}

/// A 409 when the branch already extends the server's head (commits simply
/// not pushed yet) is benign: no new commit, no head movement.
#[tokio::test(flavor = "current_thread")]
async fn pull_409_when_local_is_ahead_is_a_noop() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    // The server's branch head is the branch's own ancestor: old_tip's
    // parent chain contains it after we add one more local commit.
    let extra_blob = repo.put_blob(b"more\n".to_vec()).unwrap();
    let manifest = repo
        .put_manifest(vec![ManifestEntry {
            path: "more.txt".to_string(),
            blob_hash: extra_blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let tip2 = repo
        .put_commit(
            "tester".to_string(),
            Some(old_tip.clone()),
            None,
            manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("tester", &tip2).unwrap();
    repo.set_head(&tip2).unwrap();

    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;
    // Main refresh finds no main head; branch head is our own ancestor.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": old_tip.to_string()
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&tip2),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("ahead-of-remote 409 is benign");
    let captured = output::end_capture();

    assert!(captured.contains("Already up to date"), "got: {captured:?}");
    assert_eq!(repo.get_branch_head("tester").unwrap(), Some(tip2));
}

/// `pull --force` parks the diverged local state as an orphaned branch
/// before re-syncing — commits stay reachable, nothing is deleted.
#[tokio::test(flavor = "current_thread")]
async fn pull_force_parks_local_commits_instead_of_discarding() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    // Forced pull returns the remote's branch state: one unrelated commit
    // with an empty tree.
    let remote_manifest = oak_core::Tree::empty_hash();
    let remote_timestamp = chrono::DateTime::from_timestamp(1_700_000_700, 0).unwrap();
    let remote_commit = Commit::with_timestamp(
        "tester".to_string(),
        None,
        None,
        remote_manifest.clone(),
        "other".to_string(),
        None,
        Vec::new(),
        remote_timestamp,
    )
    .unwrap()
    .hash;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": remote_commit.as_str(),
            "branch": {
                "name": "tester",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            },
            "branches": [],
            "commits": [{
                "hash": remote_commit.as_str(),
                "branch_name": "tester",
                "parent_hash": null,
                "manifest_hash": remote_manifest.as_str(),
                "author": "other",
                "timestamp": remote_timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        true,
        temp.path(),
        None,
    )
    .await
    .expect("forced pull should succeed");
    let captured = output::end_capture();

    assert!(
        captured.contains("Parked local commits as 'tester.orphaned-"),
        "expected the keepsafe line, got: {captured:?}"
    );
    assert!(
        !captured.contains("discarding local commits"),
        "force pull must not claim to discard, got: {captured:?}"
    );

    // The branch followed the remote; the orphan keeps the old tip reachable.
    assert_eq!(
        repo.get_branch_head("tester")
            .unwrap()
            .map(|h| h.to_string()),
        Some(remote_commit.to_string())
    );
    let orphan = repo
        .list_branches()
        .unwrap()
        .into_iter()
        .find(|b| b.name.starts_with("tester.orphaned-"))
        .expect("orphan branch row must exist");
    assert_eq!(
        repo.get_branch_head(&orphan.name).unwrap(),
        Some(old_tip.clone())
    );
    assert!(repo.get_commit(&old_tip).unwrap().is_some());
}

/// Regression: a fast-forward pull must NOT silently clobber uncommitted edits.
/// Before the guard, `update_working_dir` rewrote every manifest entry over the
/// working tree, destroying in-progress work — the common "edit files, then
/// `oak pull`" case, which agents run unattended. The pull must refuse with
/// `DirtyWorkingTree`, leave the edits on disk, and move no pointers.
#[tokio::test(flavor = "current_thread")]
async fn pull_fast_forward_refuses_to_clobber_uncommitted_edits() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    // Local uncommitted edit to a tracked file.
    std::fs::write(temp.path().join("ours.txt"), "MY UNCOMMITTED WORK\n").unwrap();

    // Server fast-forwards `tester` to a new commit (parented on our tip).
    let remote_manifest = oak_core::Tree::empty_hash();
    let remote_timestamp = chrono::DateTime::from_timestamp(1_700_000_800, 0).unwrap();
    let remote_commit = Commit::with_timestamp(
        "tester".to_string(),
        Some(old_tip.clone()),
        None,
        remote_manifest.clone(),
        "other".to_string(),
        None,
        Vec::new(),
        remote_timestamp,
    )
    .unwrap()
    .hash;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": remote_commit.as_str(),
            "branch": {
                "name": "tester",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            },
            "branches": [],
            "commits": [{
                "hash": remote_commit.as_str(),
                "branch_name": "tester",
                "parent_hash": old_tip.to_string(),
                "manifest_hash": remote_manifest.as_str(),
                "author": "other",
                "timestamp": remote_timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    let err = oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect_err("a fast-forward pull over a dirty tree must refuse");
    let _ = output::end_capture();

    assert!(
        matches!(err, OakError::DirtyWorkingTree(_)),
        "expected DirtyWorkingTree, got: {err:?}"
    );
    // The uncommitted edit survives untouched.
    assert_eq!(
        std::fs::read(temp.path().join("ours.txt")).unwrap(),
        b"MY UNCOMMITTED WORK\n",
        "the dirty edit must not be overwritten"
    );
    // No pointer advanced — the refused pull is a clean, retryable no-op.
    assert_eq!(
        repo.get_branch_head("tester").unwrap(),
        Some(old_tip),
        "branch head must not advance when the pull is refused"
    );
}

/// Counterpart to the guard test: `oak pull --force` deliberately overwrites a
/// dirty tree (after parking any local-only commits). The empty-manifest
/// fast-forward deletes the tracked `ours.txt`, proving force bypasses the
/// dirty-tree guard rather than erroring.
#[tokio::test(flavor = "current_thread")]
async fn pull_force_overwrites_dirty_tree() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    std::fs::write(temp.path().join("ours.txt"), "MY UNCOMMITTED WORK\n").unwrap();

    let remote_manifest = oak_core::Tree::empty_hash();
    let remote_timestamp = chrono::DateTime::from_timestamp(1_700_000_900, 0).unwrap();
    let remote_commit = Commit::with_timestamp(
        "tester".to_string(),
        Some(old_tip.clone()),
        None,
        remote_manifest.clone(),
        "other".to_string(),
        None,
        Vec::new(),
        remote_timestamp,
    )
    .unwrap()
    .hash;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": remote_commit.as_str(),
            "branch": {
                "name": "tester",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            },
            "branches": [],
            "commits": [{
                "hash": remote_commit.as_str(),
                "branch_name": "tester",
                "parent_hash": old_tip.to_string(),
                "manifest_hash": remote_manifest.as_str(),
                "author": "other",
                "timestamp": remote_timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        true,
        temp.path(),
        None,
    )
    .await
    .expect("forced pull must overwrite the dirty tree, not error");
    let _ = output::end_capture();

    // The empty target manifest removed the tracked file: force took effect.
    assert!(
        !temp.path().join("ours.txt").exists(),
        "force pull should have applied the upstream deletion"
    );
}

/// A server that answers 200 (not 409) with a head that does NOT descend
/// from the local tip — loopback `oak serve` before the fix, released local
/// servers, or hosted's permissive deep-history walk — must not strand the
/// local tip. The unpushed commit stays checked out; nothing is parked
/// because nothing moved.
#[tokio::test(flavor = "current_thread")]
async fn pull_200_ancestor_head_keeps_unpushed_local_commit() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, pushed) = seed_diverged_workdir(&temp, &server.uri());
    let pushed_commit = repo.get_commit(&pushed).unwrap().unwrap();

    // One unpushed local commit on top of the pushed one.
    let local_blob = repo.put_blob(b"local\n".to_vec()).unwrap();
    let ours_entry = repo
        .get_manifest(&pushed_commit.manifest_hash)
        .unwrap()
        .unwrap()
        .entries;
    let mut entries = ours_entry.clone();
    entries.push(ManifestEntry {
        path: "local.txt".to_string(),
        blob_hash: local_blob,
        mode: FileMode::Regular,
    });
    let manifest = repo.put_manifest(entries).unwrap();
    let local_tip = repo
        .put_commit(
            "tester".to_string(),
            Some(pushed.clone()),
            None,
            manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("tester", &local_tip).unwrap();
    repo.set_head(&local_tip).unwrap();
    std::fs::write(temp.path().join("local.txt"), "local\n").unwrap();

    // Legacy answer to an unknown `since`: 200 + the branch's full history.
    let (root, wire_trees) = wire_tree_fixture(ours_entry);
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": pushed.as_str(),
            "branch": {
                "name": "tester",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            },
            "branches": [],
            "commits": [{
                "hash": pushed.as_str(),
                "branch_name": "tester",
                "parent_hash": null,
                "manifest_hash": root,
                "author": pushed_commit.author,
                "timestamp": pushed_commit.timestamp.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                "files": []
            }],
            "blobs": [],
            "trees": wire_trees
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Convergence probe: no main; the server branch head is our ancestor.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": pushed.to_string()
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&local_tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("an ahead-of-remote pull is benign");
    let captured = output::end_capture();

    assert_eq!(
        repo.get_branch_head("tester").unwrap(),
        Some(local_tip.clone()),
        "branch must stay on the unpushed commit; got output {captured:?}"
    );
    assert_eq!(repo.get_head().unwrap(), Some(local_tip));
    assert_eq!(
        std::fs::read(temp.path().join("local.txt")).unwrap(),
        b"local\n"
    );
    assert!(
        captured.contains("does not contain local commit")
            && captured.contains("Already up to date (local commits not pushed yet)"),
        "must explain that local commits were kept: {captured:?}"
    );
}

/// Same legacy 200, but the remote branch really moved elsewhere: converge by
/// re-parenting (old tip kept as merge parent), exactly like a hosted 409.
#[tokio::test(flavor = "current_thread")]
async fn pull_200_diverged_head_reparents_instead_of_discarding() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

    let theirs_content = b"theirs\n";
    let theirs_blob = oak_core::hash_bytes(theirs_content);
    let (root, wire_trees) = wire_tree_fixture(vec![ManifestEntry {
        path: "theirs.txt".to_string(),
        blob_hash: theirs_blob,
        mode: FileMode::Regular,
    }]);
    let seed_timestamp = chrono::DateTime::from_timestamp(1_700_000_800, 0).unwrap();
    let seed = Commit::with_timestamp(
        "main".to_string(),
        None,
        None,
        Hash(root.clone()),
        "<remote>".to_string(),
        Some("merged something".to_string()),
        Vec::new(),
        seed_timestamp,
    )
    .unwrap()
    .hash;
    // The legacy 200: the remote head is unrelated to the local tip.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str(),
            "branch": null,
            "branches": [],
            "commits": [{
                "hash": seed.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": root,
                "author": "<remote>",
                "message": "merged something",
                "timestamp": seed_timestamp.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": wire_trees
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "commits": [{
                "hash": seed.as_str(),
                "branch_name": "main",
                "parent_hash": null,
                "manifest_hash": root,
                "author": "<remote>",
                "message": "merged something",
                "timestamp": seed_timestamp.to_rfc3339(),
                "files": []
            }],
            "trees": wire_trees
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/oak/oak/raw/{}/theirs.txt",
            seed.as_str()
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(theirs_content.to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": seed.as_str()
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&old_tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("diverged 200 must converge");
    let captured = output::end_capture();

    let new_head = repo.get_branch_head("tester").unwrap().unwrap();
    assert_ne!(
        new_head, seed,
        "must not reset onto the remote head: {captured:?}"
    );
    let commit = repo.get_commit(&new_head).unwrap().unwrap();
    assert_eq!(commit.parent_hash, Some(seed));
    assert_eq!(commit.merge_parent_hash, Some(old_tip));
    assert!(temp.path().join("ours.txt").exists());
    assert!(temp.path().join("theirs.txt").exists());
    assert!(
        captured.contains("Re-parented 'tester'"),
        "got: {captured:?}"
    );
}

/// Publication never regresses a NON-selected local branch whose head the
/// response does not descend from (a response may carry commits labelled
/// with other branch names).
#[tokio::test(flavor = "current_thread")]
async fn pull_does_not_regress_other_local_branch_heads() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, tip) = seed_diverged_workdir(&temp, &server.uri());

    // Local-only work on another branch.
    repo.store_branch(&Branch::new(
        "side".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    let side_blob = repo.put_blob(b"side\n".to_vec()).unwrap();
    let side_manifest = repo
        .put_manifest(vec![ManifestEntry {
            path: "side.txt".to_string(),
            blob_hash: side_blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let side_tip = repo
        .put_commit(
            "side".to_string(),
            None,
            None,
            side_manifest,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_branch_head("side", &side_tip).unwrap();

    // Response: nothing new for `tester`, plus an unrelated `side` commit.
    let remote_manifest = oak_core::Tree::empty_hash();
    let ts = chrono::DateTime::from_timestamp(1_700_000_900, 0).unwrap();
    let remote_side = Commit::with_timestamp(
        "side".to_string(),
        None,
        None,
        remote_manifest.clone(),
        "other".to_string(),
        None,
        Vec::new(),
        ts,
    )
    .unwrap()
    .hash;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": tip.as_str(),
            "branch": null,
            "branches": [],
            "commits": [{
                "hash": remote_side.as_str(),
                "branch_name": "side",
                "parent_hash": null,
                "manifest_hash": remote_manifest.as_str(),
                "author": "other",
                "timestamp": ts.to_rfc3339(),
                "files": []
            }],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&tip),
        false,
        temp.path(),
        None,
    )
    .await
    .expect("pull succeeds");
    output::end_capture();

    assert_eq!(repo.get_branch_head("side").unwrap(), Some(side_tip));
    assert_eq!(repo.get_branch_head("tester").unwrap(), Some(tip));
}

// ---------------------------------------------------------------------------
// QA-L9-A reviewer tests (not part of the candidate).
// ---------------------------------------------------------------------------

/// F1: hosted/serve `get_commits_since` ships EVERY commit labeled with the
/// branch (timestamp-ordered), including a stray one left behind by a
/// force-push or an ancestor with a skewed clock. The response head is a plain
/// fast-forward of the local tip, so the pull must succeed and land on it —
/// the last-seen same-branch commit is not the branch head.
#[tokio::test(flavor = "current_thread")]
async fn qa_fast_forward_pull_ignores_stray_same_branch_commit_in_response() {
    let temp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (repo, local_tip) = seed_diverged_workdir(&temp, &server.uri());
    let local_commit = repo.get_commit(&local_tip).unwrap().unwrap();
    let entries = repo
        .get_manifest(&local_commit.manifest_hash)
        .unwrap()
        .unwrap()
        .entries;
    let (root, wire_trees) = wire_tree_fixture(entries);
    let t_ff = chrono::DateTime::from_timestamp(1_900_000_000, 0).unwrap();
    let t_stray = chrono::DateTime::from_timestamp(1_900_000_100, 0).unwrap();
    // Remote head: fast-forward child of the local tip.
    let ff = Commit::with_timestamp(
        "tester".to_string(),
        Some(local_tip.clone()),
        None,
        Hash(root.clone()),
        "other".to_string(),
        Some("ff".to_string()),
        Vec::new(),
        t_ff,
    )
    .unwrap()
    .hash;
    // Stray commit labeled `tester`, not an ancestor/descendant of the tip,
    // newer timestamp (e.g. a force-pushed-away tip still in the table).
    let stray = Commit::with_timestamp(
        "tester".to_string(),
        None,
        None,
        Hash(root.clone()),
        "other".to_string(),
        Some("stray".to_string()),
        Vec::new(),
        t_stray,
    )
    .unwrap()
    .hash;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": ff.as_str(),
            "branch": {
                "name": "tester",
                "description": null,
                "parent_branch": "main",
                "status": "open",
                "created_at": Utc::now().to_rfc3339(),
            },
            "branches": [],
            "commits": [
                {
                    "hash": ff.as_str(), "branch_name": "tester",
                    "parent_hash": local_tip.as_str(), "manifest_hash": root,
                    "author": "other", "message": "ff",
                    "timestamp": t_ff.to_rfc3339(), "files": []
                },
                {
                    "hash": stray.as_str(), "branch_name": "tester",
                    "parent_hash": null, "manifest_hash": root,
                    "author": "other", "message": "stray",
                    "timestamp": t_stray.to_rfc3339(), "files": []
                }
            ],
            "blobs": [],
            "trees": wire_trees
        })))
        .mount(&server)
        .await;

    let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
    output::begin_capture();
    let result = oak_cli::commands::pull::pull_async(
        &lock,
        &repo,
        &server.uri(),
        "oak/oak/pull",
        Some("tester"),
        Some(&local_tip),
        false,
        temp.path(),
        None,
    )
    .await;
    let captured = output::end_capture();
    assert!(
        result.is_ok(),
        "fast-forward pull must succeed: {result:?} {captured:?}"
    );
    assert_eq!(repo.get_branch_head("tester").unwrap(), Some(ff.clone()));
    assert_eq!(repo.get_head().unwrap(), Some(ff));
}

/// Code-owner ask: a 404 from `/blobs/info` caused by access denial must not
/// become a silent partial hydrate via the branch-scoped legacy fallback. The
/// legacy pull is denied too (404): the pull fails loudly, no ref moves, the
/// local commit and worktree are untouched, no sync/conflict state is left.
#[tokio::test(flavor = "current_thread")]
async fn qa_diverged_pull_access_denied_blobs_info_404_fails_loudly_without_ref_move() {
    for first_status in [200u16, 409u16] {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let (repo, old_tip) = seed_diverged_workdir(&temp, &server.uri());

        let theirs_blob = oak_core::hash_bytes(b"theirs\n");
        let (root, wire_trees) = wire_tree_fixture(vec![ManifestEntry {
            path: "theirs.txt".to_string(),
            blob_hash: theirs_blob,
            mode: FileMode::Regular,
        }]);
        let ts = chrono::DateTime::from_timestamp(1_700_000_800, 0).unwrap();
        let seed = Commit::with_timestamp(
            "tester".to_string(),
            None,
            None,
            Hash(root.clone()),
            "<remote>".to_string(),
            Some("remote work".to_string()),
            Vec::new(),
            ts,
        )
        .unwrap()
        .hash;
        let commit_json = serde_json::json!({
            "hash": seed.as_str(), "branch_name": "tester", "parent_hash": null,
            "manifest_hash": root, "author": "<remote>", "message": "remote work",
            "timestamp": ts.to_rfc3339(), "files": []
        });
        let first = if first_status == 200 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": seed.as_str(),
                "branch": {"name": "tester", "description": null, "parent_branch": "main",
                           "status": "open", "created_at": Utc::now().to_rfc3339()},
                "branches": [], "commits": [commit_json.clone()],
                "blobs": [], "trees": wire_trees
            }))
        } else {
            ResponseTemplate::new(409)
        };
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/pull"))
            .respond_with(first)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Every later request the credential is not allowed to make: 404.
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/pull"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "Not found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/blobs/info"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": "Not found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [commit_json.clone()], "trees": wire_trees
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/branches/tester"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": seed.as_str()
            })))
            .mount(&server)
            .await;

        let lock = oak_cli::workdir_lock::WorkdirLock::acquire(&temp.path().join(".oak")).unwrap();
        output::begin_capture();
        let result = oak_cli::commands::pull::pull_async(
            &lock,
            &repo,
            &server.uri(),
            "oak/oak/pull",
            Some("tester"),
            Some(&old_tip),
            false,
            temp.path(),
            None,
        )
        .await;
        let captured = output::end_capture();
        let requests: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect();
        eprintln!("[{first_status}] result={result:?}\n requests={requests:?}\n out={captured:?}");
        assert!(
            result.is_err(),
            "[{first_status}] denied hydration must fail loudly: {captured:?}"
        );
        assert_eq!(
            repo.get_branch_head("tester").unwrap(),
            Some(old_tip.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(old_tip.clone()));
        assert!(!repo.has_blob(&oak_core::hash_bytes(b"theirs\n")).unwrap());
        assert_eq!(
            std::fs::read(temp.path().join("ours.txt")).unwrap(),
            b"ours\n"
        );
        assert!(!temp.path().join("theirs.txt").exists());
        assert!(
            !temp.path().join(".oak/SYNC_HEAD").exists()
                && !temp.path().join(".oak/SYNC_STATE").exists(),
            "[{first_status}] no sync state may be left behind"
        );
        assert!(
            requests.iter().any(|r| r.ends_with("/blobs/info")),
            "[{first_status}] must have attempted /blobs/info: {requests:?}"
        );
    }
}
