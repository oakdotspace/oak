use std::process::Command;

use oak_core::{
    Branch, FileMode, ManifestEntry, MetadataKey, Repository, SqliteRepository, StatCacheEntry,
};

fn fixture() -> (tempfile::TempDir, oak_core::Hash) {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.space")
        .unwrap();
    let base_blob = repo.put_blob(b"base bytes\n".to_vec()).unwrap();
    let tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "tracked.txt".into(),
            blob_hash: base_blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let head = repo
        .put_commit(
            "topic".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("topic").unwrap();
    repo.set_branch_head("topic", &head).unwrap();
    repo.set_head(&head).unwrap();
    std::fs::write(dir.path().join("tracked.txt"), b"captured bytes\n").unwrap();

    // A deliberately false cache hit: capture must bypass and preserve it.
    let metadata = std::fs::metadata(dir.path().join("tracked.txt")).unwrap();
    #[cfg(unix)]
    let (mtime_ns, ctime_ns) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
        )
    };
    #[cfg(not(unix))]
    let (mtime_ns, ctime_ns) = (0, 0);
    repo.update_stat_cache(
        &[(
            "tracked.txt".into(),
            StatCacheEntry {
                mtime_ns,
                ctime_ns,
                size: metadata.len() as i64,
                blob_hash: oak_core::hash_bytes(b"poisoned cache bytes\n"),
            },
        )],
        &[],
    )
    .unwrap();
    (dir, head)
}

fn capture(dir: &std::path::Path, paths: &[&str]) -> (std::process::Output, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "capture", "--json"])
        .args(paths)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    parse_capture_output(output)
}

fn parse_capture_output(output: std::process::Output) -> (std::process::Output, serde_json::Value) {
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, json)
}

#[cfg(unix)]
fn capture_without_read_privilege(
    dir: &std::path::Path,
) -> (std::process::Output, serde_json::Value) {
    // CI may run as root, which can read a chmod(000) file. Exercise the real
    // permission-denied path with an unprivileged child instead of skipping
    // the test or claiming a read failure the OS did not produce.
    if unsafe { libc::geteuid() } != 0 {
        return capture(dir, &[]);
    }
    use std::os::unix::process::CommandExt;
    fn give_fixture_to_reader(path: &std::path::Path) {
        std::os::unix::fs::chown(path, Some(65534), Some(65534)).unwrap();
        if path.is_dir() {
            for child in std::fs::read_dir(path).unwrap() {
                give_fixture_to_reader(&child.unwrap().path());
            }
        }
    }
    // The source checkout may be beneath a root-only ancestor. Put the
    // executable inside the temporary fixture's excluded metadata directory.
    let binary = dir.join(".oak/permission-test-oak");
    std::fs::copy(env!("CARGO_BIN_EXE_oak"), &binary).unwrap();
    give_fixture_to_reader(dir);
    let output = Command::new(binary)
        .args(["change", "capture", "--json"])
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .gid(65534)
        .uid(65534)
        .output()
        .unwrap();
    parse_capture_output(output)
}

#[test]
fn capture_persists_immutable_bytes_without_changing_head_or_stat_cache() {
    let (dir, head) = fixture();
    let before_cache = SqliteRepository::open(&dir.path().join(".oak/oak.db"))
        .unwrap()
        .load_stat_cache()
        .unwrap();
    let before_commit_count = SqliteRepository::open(&dir.path().join(".oak/oak.db"))
        .unwrap()
        .get_all_commits()
        .unwrap()
        .len();

    let (output, receipt) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["kind"], "working_tree_change_capture");
    assert_eq!(receipt["repository"]["owner"], "acme");
    assert_eq!(receipt["repository"]["name"], "widgets");
    assert_eq!(receipt["repository"]["origin"], "https://oak.space");
    assert_eq!(receipt["base"]["commit"], head.as_str());
    assert_eq!(receipt["scope"]["kind"], "full");
    assert_eq!(
        receipt["consistency"]["model"],
        "non_atomic_working_tree_capture"
    );
    assert_eq!(receipt["consistency"]["external_writers_excluded"], false);
    assert_eq!(receipt["storage"]["blobs"], "durable_local_sqlite");
    assert_eq!(receipt["storage"]["refs_changed"], false);

    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert_eq!(repo.get_branch_head("topic").unwrap(), Some(head));
    assert_eq!(repo.load_stat_cache().unwrap(), before_cache);
    assert_eq!(repo.get_all_commits().unwrap().len(), before_commit_count);
    assert_eq!(
        std::fs::read(dir.path().join("tracked.txt")).unwrap(),
        b"captured bytes\n"
    );
    let result_tree =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_tree).unwrap().unwrap();
    let captured_blob = result.get("tracked.txt").unwrap().blob_hash.clone();
    std::fs::write(dir.path().join("tracked.txt"), b"later bytes\n").unwrap();
    assert_eq!(
        repo.get_blob(&captured_blob).unwrap().unwrap().content,
        b"captured bytes\n"
    );
}

#[test]
fn repeated_and_cross_branch_captures_keep_distinct_occurrence_provenance() {
    let (dir, head) = fixture();
    let (first_output, first) = capture(dir.path(), &[]);
    let (repeat_output, repeat) = capture(dir.path(), &[]);
    assert!(first_output.status.success(), "{first}");
    assert!(repeat_output.status.success(), "{repeat}");
    assert_eq!(first["change_set"]["id"], repeat["change_set"]["id"]);
    assert_ne!(first["capture_id"], repeat["capture_id"]);

    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.store_branch(&Branch::new("other".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("other", &head).unwrap();
    repo.set_current_branch("other").unwrap();
    drop(repo);

    let (other_output, other) = capture(dir.path(), &[]);
    assert!(other_output.status.success(), "{other}");
    assert_eq!(first["change_set"]["id"], other["change_set"]["id"]);
    assert_ne!(first["capture_id"], other["capture_id"]);
    assert_ne!(repeat["capture_id"], other["capture_id"]);

    let reopened = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let first_record = reopened
        .get_change_capture(first["capture_id"].as_str().unwrap())
        .unwrap();
    let repeat_record = reopened
        .get_change_capture(repeat["capture_id"].as_str().unwrap())
        .unwrap();
    let other_record = reopened
        .get_change_capture(other["capture_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(first_record.provenance.source_branch, "topic");
    assert_eq!(repeat_record.provenance.source_branch, "topic");
    assert_eq!(other_record.provenance.source_branch, "other");
    for record in [&first_record, &repeat_record, &other_record] {
        assert_eq!(record.provenance.owner, "acme");
        assert_eq!(record.provenance.name, "widgets");
        assert_eq!(
            record.provenance.origin.as_deref(),
            Some("https://oak.space")
        );
        assert_eq!(record.provenance.base_commit.as_ref(), Some(&head));
        assert_eq!(record.change_set.id().as_str(), first["change_set"]["id"]);
    }
}

#[test]
fn capture_accepts_a_sanitized_origin_with_at_in_its_path() {
    let (dir, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(
        MetadataKey::RemoteUrl,
        "https://example.invalid/@tenant/path",
    )
    .unwrap();
    drop(repo);

    let (output, receipt) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(
        receipt["repository"]["origin"],
        "https://example.invalid/@tenant/path"
    );
}

#[test]
fn capture_preserves_the_existing_long_branch_name_contract() {
    let (dir, head) = fixture();
    let branch = "b".repeat(256);
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.store_branch(&Branch::new(branch.clone(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head(&branch, &head).unwrap();
    repo.set_current_branch(&branch).unwrap();
    drop(repo);

    let (output, receipt) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["base"]["branch"], branch);
}

#[test]
fn capture_reports_an_incomplete_base_without_scanning_the_worktree() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
    let missing_tree = oak_core::Hash("62".repeat(32));
    let head = repo
        .put_commit(
            "topic".into(),
            None,
            None,
            missing_tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("topic").unwrap();
    repo.set_foreign_keys(false).unwrap();
    repo.set_branch_head("topic", &head).unwrap();
    repo.set_head(&head).unwrap();
    std::fs::write(dir.path().join("untracked.txt"), b"must not be stored").unwrap();
    let untracked_hash = oak_core::hash_bytes(b"must not be stored");
    drop(repo);

    let (output, error) = capture(dir.path(), &[]);

    assert!(!output.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "incomplete_manifest_data");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert!(!repo.has_blob(&untracked_hash).unwrap());
}

#[test]
fn scoped_capture_carries_sparse_and_declared_unavailable_base_entries() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".oak")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
    repo.set_metadata(MetadataKey::SparsePaths, "src").unwrap();
    let old_src = repo.put_blob(b"old source\n".to_vec()).unwrap();
    let sparse_hash = oak_core::Hash("71".repeat(32));
    let known_lost_hash = oak_core::Hash("72".repeat(32));
    repo.set_metadata(MetadataKey::KnownLostBlobs, known_lost_hash.as_str())
        .unwrap();
    let tree = repo
        .put_manifest(vec![
            ManifestEntry {
                path: "src/lib.rs".into(),
                blob_hash: old_src,
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "vendor/sparse.bin".into(),
                blob_hash: sparse_hash.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "lost.txt".into(),
                blob_hash: known_lost_hash.clone(),
                mode: FileMode::Regular,
            },
        ])
        .unwrap();
    let head = repo
        .put_commit(
            "topic".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("topic").unwrap();
    repo.set_branch_head("topic", &head).unwrap();
    repo.set_head(&head).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), b"new source\n").unwrap();
    drop(repo);

    let (output, receipt) = capture(dir.path(), &["src"]);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["kind"], "paths");
    assert_eq!(receipt["scope"]["paths"], serde_json::json!(["src"]));
    assert_eq!(receipt["scope"]["paths_complete"], true);
    assert_eq!(receipt["scope"]["effective"]["sparse_cone_active"], true);
    assert_eq!(receipt["scope"]["effective"]["sparse_prefix_count"], 1);
    assert_eq!(
        receipt["scope"]["effective"]["known_loss_base_paths_excluded"], 0,
        "known-loss path outside requested src scope is preserved but not an effective exclusion"
    );
    assert_eq!(
        receipt["change_set"]["changes"].as_array().unwrap().len(),
        1
    );
    let result_tree =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let result = repo.get_manifest(&result_tree).unwrap().unwrap();
    assert_eq!(
        result.get("vendor/sparse.bin").unwrap().blob_hash,
        sparse_hash
    );
    assert_eq!(result.get("lost.txt").unwrap().blob_hash, known_lost_hash);
    assert_eq!(
        repo.get_blob(&result.get("src/lib.rs").unwrap().blob_hash)
            .unwrap()
            .unwrap()
            .content,
        b"new source\n"
    );
}

#[test]
fn capture_refuses_a_future_repository_object_format() {
    let (dir, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::HashFormat, "v2").unwrap();
    let dirty_hash = oak_core::hash_bytes(b"captured bytes\n");
    drop(repo);

    let (output, error) = capture(dir.path(), &[]);

    assert!(!output.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "unsupported");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert!(!repo.has_blob(&dirty_hash).unwrap());
}

#[test]
fn scoped_capture_selects_paths_before_reading_or_updating_the_result() {
    let (dir, _) = fixture();
    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
    std::fs::write(dir.path().join("new.txt"), b"base bytes\n").unwrap();
    let unrelated = b"unselected unique bytes";
    std::fs::write(dir.path().join("unselected.txt"), unrelated).unwrap();

    let (output, receipt) = capture(dir.path(), &["new.txt"]);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["paths"], serde_json::json!(["new.txt"]));
    assert_eq!(
        receipt["change_set"]["changes"].as_array().unwrap().len(),
        1
    );
    assert_eq!(receipt["change_set"]["changes"][0]["path"], "new.txt");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let result_hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_hash).unwrap().unwrap();
    assert!(result.get("tracked.txt").is_some());
    assert!(result.get("new.txt").is_some());
    assert!(!repo.has_blob(&oak_core::hash_bytes(unrelated)).unwrap());
}

#[test]
fn full_capture_preserves_unavailable_base_paths_before_matching_new_content() {
    for marker in [MetadataKey::RestrictedBlobs, MetadataKey::KnownLostBlobs] {
        let (dir, _) = fixture();
        std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        let base_hash = oak_core::hash_bytes(b"base bytes\n");
        repo.set_metadata(marker, base_hash.as_str()).unwrap();
        drop(repo);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute("DELETE FROM blobs WHERE hash=?1", [base_hash.as_str()])
            .unwrap();
        drop(conn);
        std::fs::write(dir.path().join("new.txt"), b"base bytes\n").unwrap();

        let (output, receipt) = capture(dir.path(), &[]);

        assert!(output.status.success(), "{receipt}");
        assert_eq!(
            receipt["scope"]["effective"]["unavailable_base_paths_excluded"],
            1
        );
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        let result_hash =
            oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap())
                .unwrap();
        let result = repo.get_manifest(&result_hash).unwrap().unwrap();
        assert!(result.get("tracked.txt").is_some());
        assert!(result.get("new.txt").is_some());
    }
}

#[test]
fn full_capture_preserves_out_of_cone_base_paths() {
    let (dir, _) = fixture();
    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::SparsePaths, "src").unwrap();
    drop(repo);
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/new.txt"), b"base bytes\n").unwrap();

    let (output, receipt) = capture(dir.path(), &[]);

    assert!(output.status.success(), "{receipt}");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let result_hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_hash).unwrap().unwrap();
    assert!(result.get("tracked.txt").is_some());
    assert!(result.get("src/new.txt").is_some());
}

#[cfg(unix)]
#[test]
fn scoped_capture_resolves_a_selected_symlink_lexically() {
    let (dir, _) = fixture();
    std::os::unix::fs::symlink("tracked.txt", dir.path().join("link")).unwrap();

    let (output, receipt) = capture(dir.path(), &["link"]);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["paths"], serde_json::json!(["link"]));
    assert_eq!(receipt["change_set"]["changes"][0]["path"], "link");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let result_hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_hash).unwrap().unwrap();
    let link = result.get("link").unwrap();
    assert_eq!(link.mode, FileMode::Symlink);
    assert_eq!(
        repo.get_blob(&link.blob_hash).unwrap().unwrap().content,
        b"tracked.txt"
    );
}

#[test]
fn capture_rejects_an_inconsistent_existing_blob_row() {
    let (dir, _) = fixture();
    let expected = oak_core::hash_bytes(b"captured bytes\n");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.store_blob(&oak_core::Blob {
        hash: expected,
        content: b"wrong bytes".to_vec(),
        size: 11,
    })
    .unwrap();
    drop(repo);

    let (output, error) = capture(dir.path(), &[]);

    assert!(!output.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "captured_blob_integrity");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert_eq!(
        repo.get_blob(&oak_core::hash_bytes(b"captured bytes\n"))
            .unwrap()
            .unwrap()
            .content,
        b"wrong bytes",
        "capture must not silently repair an inconsistent customer row"
    );
}

#[test]
fn capture_rejects_an_inconsistent_existing_result_tree_row() {
    let (dir, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let expected_result = oak_core::build_tree(&[ManifestEntry {
        path: "tracked.txt".into(),
        blob_hash: oak_core::hash_bytes(b"captured bytes\n"),
        mode: FileMode::Regular,
    }])
    .unwrap()
    .root_hash;
    let wrong_blob = repo.put_blob(b"wrong tree bytes".to_vec()).unwrap();
    let wrong_tree = repo
        .put_tree(vec![ManifestEntry {
            path: "wrong.txt".into(),
            blob_hash: wrong_blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    drop(repo);
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    let wrong_content: Vec<u8> = conn
        .query_row(
            "SELECT content FROM trees WHERE hash=?1",
            [wrong_tree.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO trees (hash, content) VALUES (?1, ?2)",
        rusqlite::params![expected_result.as_str(), wrong_content],
    )
    .unwrap();
    drop(conn);

    let (output, error) = capture(dir.path(), &[]);

    assert!(!output.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "captured_tree_integrity");
}

#[test]
fn capture_receipt_redacts_origin_credentials_and_non_authority_components() {
    let (dir, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(
        MetadataKey::RemoteUrl,
        "https://fake-user:fake-password@example.invalid/path?token=fake-token#fake-fragment",
    )
    .unwrap();
    drop(repo);

    let (output, receipt) = capture(dir.path(), &[]);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(
        receipt["repository"]["origin"],
        "https://example.invalid/path"
    );
    let encoded = serde_json::to_string(&receipt).unwrap();
    for secret in ["fake-user", "fake-password", "fake-token", "fake-fragment"] {
        assert!(!encoded.contains(secret), "receipt leaked {secret}");
    }
}

#[test]
fn capture_receipt_bounds_summaries_but_identity_covers_every_selected_change() {
    let (dir, head) = fixture();
    let paths: Vec<String> = (0..80)
        .map(|index| format!("many/{index:03}.txt"))
        .collect();
    std::fs::create_dir(dir.path().join("many")).unwrap();
    for path in &paths {
        std::fs::write(dir.path().join(path), path.as_bytes()).unwrap();
    }
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();

    let (output, receipt) = capture(dir.path(), &path_refs);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["path_count"], 80);
    assert!(receipt["scope"]["paths"].as_array().unwrap().len() < 80);
    assert_eq!(receipt["scope"]["paths_complete"], false);
    assert!(receipt["scope"]["paths_omitted"].as_u64().unwrap() > 0);
    assert_eq!(receipt["change_set"]["change_count"], 80);
    assert!(receipt["change_set"]["changes"].as_array().unwrap().len() < 80);
    assert_eq!(receipt["change_set"]["changes_complete"], false);
    assert!(receipt["change_set"]["changes_omitted"].as_u64().unwrap() > 0);
    assert_eq!(
        receipt["change_set"]["identity_covers"],
        "complete_change_set"
    );
    assert_eq!(receipt["lookup"], "durable_local_sqlite");
    let capture_id = receipt["capture_id"]
        .as_str()
        .expect("successful captures expose an occurrence ID");

    // Lookup resolves the immutable stored descriptor, never a later worktree
    // scan. Removing every captured path after publication must not change it.
    std::fs::remove_dir_all(dir.path().join("many")).unwrap();

    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let base_commit = repo.get_commit(&head).unwrap().unwrap();
    let base = repo
        .get_manifest(&base_commit.manifest_hash)
        .unwrap()
        .unwrap();
    let result_hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_hash).unwrap().unwrap();
    let reconstructed = oak_core::CanonicalChangeSetV1::from_manifests(
        &base,
        &result,
        oak_core::ChangeScopeV1::Paths { paths },
    )
    .unwrap();
    assert_eq!(receipt["change_set"]["id"], reconstructed.id().as_str());

    let stored = repo
        .get_change_capture(capture_id)
        .expect("durable lookup validates stored capture");
    assert_eq!(stored.capture_id, capture_id);
    assert_eq!(stored.change_set, reconstructed);
    match stored.change_set.scope() {
        oak_core::ChangeScopeV1::Paths { paths } => assert_eq!(paths.len(), 80),
        oak_core::ChangeScopeV1::Full => panic!("expected the complete path-scoped descriptor"),
    }
    assert_eq!(stored.change_set.changes().len(), 80);
    assert_eq!(stored.provenance.owner, "acme");
    assert_eq!(stored.provenance.name, "widgets");
    assert_eq!(
        stored.provenance.origin.as_deref(),
        Some("https://oak.space")
    );
    assert_eq!(stored.provenance.source_branch, "topic");
    assert_eq!(stored.provenance.base_commit.as_ref(), Some(&head));
}

#[cfg(unix)]
#[test]
fn capture_persists_empty_binary_large_executable_and_symlink_bytes() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let (dir, _) = fixture();
    let large: Vec<u8> = (0..9 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    std::fs::write(dir.path().join("empty"), b"").unwrap();
    std::fs::write(dir.path().join("large"), &large).unwrap();
    std::fs::write(dir.path().join("exec"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        dir.path().join("exec"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("/not/a/real/target", dir.path().join("link")).unwrap();

    let (output, receipt) = capture(dir.path(), &[]);

    assert!(output.status.success(), "{receipt}");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let result_hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&result_hash).unwrap().unwrap();
    for (path, bytes, mode) in [
        ("empty", &b""[..], FileMode::Regular),
        ("large", large.as_slice(), FileMode::Regular),
        ("exec", &b"#!/bin/sh\n"[..], FileMode::Executable),
        ("link", &b"/not/a/real/target"[..], FileMode::Symlink),
    ] {
        let entry = result.get(path).unwrap();
        assert_eq!(entry.mode, mode);
        assert_eq!(
            repo.get_blob(&entry.blob_hash).unwrap().unwrap().content,
            bytes
        );
    }
}

#[cfg(unix)]
#[test]
fn capture_does_not_certify_a_file_that_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, _) = fixture();
    let path = dir.path().join("tracked.txt");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (output, error) = capture_without_read_privilege(dir.path());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        !output.status.success(),
        "read error certified as a capture"
    );
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS fixture filesystem rejects non-UTF-8 filename bytes before Oak runs"
)]
fn capture_rejects_an_unrepresentable_filename() {
    use std::os::unix::ffi::OsStringExt;

    let (dir, _) = fixture();
    let filename = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
    std::fs::write(dir.path().join(filename), b"unrepresentable path bytes").unwrap();

    let (output, _) = capture(dir.path(), &[]);

    assert!(!output.status.success(), "lossy path was certified");
}

#[test]
fn capture_retains_an_explicit_empty_directory_scope() {
    let (dir, _) = fixture();
    std::fs::create_dir(dir.path().join("empty-dir")).unwrap();

    let (output, receipt) = capture(dir.path(), &["empty-dir"]);

    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["kind"], "paths");
    assert_eq!(receipt["scope"]["paths"], serde_json::json!(["empty-dir"]));
    assert_eq!(receipt["change_set"]["change_count"], 0);
    assert_eq!(receipt["change_set"]["changes_complete"], true);
}
