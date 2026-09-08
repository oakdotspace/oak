use std::process::Command;

use std::io::Read;

use oak_core::{Branch, FileMode, ManifestEntry, MetadataKey, Repository, SqliteRepository};

fn repository_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".oak")).unwrap();
    SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    dir
}

fn captured_fixture() -> (tempfile::TempDir, serde_json::Value) {
    let dir = repository_fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.space")
        .unwrap();
    let base = repo.put_blob(b"before\n".to_vec()).unwrap();
    let tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "tracked.txt".into(),
            blob_hash: base,
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
    std::fs::write(dir.path().join("tracked.txt"), b"after\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "capture", "--json"])
        .current_dir(dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = serde_json::from_slice(&output.stdout).unwrap();
    (dir, receipt)
}

#[test]
fn unknown_capture_fails_without_creating_an_archive() {
    let dir = repository_fixture();
    let archive = dir.path().join("unknown.oakchange");
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "change",
            "export",
            "capture-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--output",
        ])
        .arg(&archive)
        .arg("--json")
        .current_dir(dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(error["error"]["code"], "change_capture_not_found");
    assert_eq!(
        error["error"]["recommended_next_commands"][0],
        "oak change capture --json"
    );
    assert!(!archive.exists());
}

#[test]
fn export_is_self_contained_and_pins_the_complete_v1_format() {
    let (dir, capture) = captured_fixture();
    std::fs::write(dir.path().join("tracked.txt"), b"later worktree bytes\n").unwrap();
    let archive_path = dir.path().join("change.oakchange");
    let capture_id = capture["capture_id"].as_str().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "export", capture_id, "--output"])
        .arg(&archive_path)
        .arg("--json")
        .current_dir(dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["kind"], "oak_change_export");
    assert_eq!(receipt["capture_id"], capture_id);
    assert_eq!(receipt["change_id"], capture["change_set"]["id"]);
    assert_eq!(receipt["blob_count"], 2);
    assert_eq!(receipt["logical_bytes"], 13);
    assert_eq!(receipt["remote_contacted"], false);

    let file = std::fs::File::open(&archive_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut names: Vec<String> = archive.file_names().map(str::to_string).collect();
    names.sort();
    let mut expected_hashes = [
        oak_core::hash_bytes(b"before\n").to_string(),
        oak_core::hash_bytes(b"after\n").to_string(),
    ];
    expected_hashes.sort();
    assert_eq!(
        names,
        vec![
            format!("blobs/{}", expected_hashes[0]),
            format!("blobs/{}", expected_hashes[1]),
            "manifest.json".to_string(),
        ]
    );

    let manifest: serde_json::Value = {
        let mut bytes = Vec::new();
        archive
            .by_name("manifest.json")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    let mut top_keys: Vec<_> = manifest.as_object().unwrap().keys().cloned().collect();
    top_keys.sort();
    assert_eq!(
        top_keys,
        ["capture", "change_set", "kind", "payload", "schema_version"]
    );
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["kind"], "oak_change_export");
    assert_eq!(manifest["capture"]["capture_id"], capture_id);
    assert_eq!(manifest["capture"]["repository"]["owner"], "acme");
    assert_eq!(manifest["capture"]["repository"]["name"], "widgets");
    assert_eq!(
        manifest["capture"]["repository"]["origin"],
        "https://oak.space"
    );
    assert_eq!(manifest["capture"]["source_branch"], "topic");
    let change_set = manifest["change_set"].as_object().unwrap();
    let mut change_set_keys: Vec<_> = change_set.keys().cloned().collect();
    change_set_keys.sort();
    assert_eq!(
        change_set_keys,
        [
            "base_tree",
            "changes",
            "id",
            "object_format",
            "result_tree",
            "schema_version",
            "scope",
        ]
    );
    assert_eq!(manifest["change_set"]["id"], capture["change_set"]["id"]);
    assert_eq!(
        manifest["change_set"]["changes"],
        capture["change_set"]["changes"]
    );
    assert_eq!(manifest["change_set"]["scope"]["kind"], "full");
    assert_eq!(manifest["payload"]["blob_count"], 2);
    assert_eq!(manifest["payload"]["logical_bytes"], 13);
    assert_eq!(manifest["payload"]["blobs"].as_array().unwrap().len(), 2);
    assert_eq!(manifest["payload"]["blobs"][0]["hash"], expected_hashes[0]);
    assert_eq!(manifest["payload"]["blobs"][1]["hash"], expected_hashes[1]);
    for (hash, bytes) in [
        (oak_core::hash_bytes(b"before\n"), b"before\n".as_slice()),
        (oak_core::hash_bytes(b"after\n"), b"after\n".as_slice()),
    ] {
        let mut actual = Vec::new();
        archive
            .by_name(&format!("blobs/{hash}"))
            .unwrap()
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, bytes);
    }
    assert_eq!(
        std::fs::read(dir.path().join("tracked.txt")).unwrap(),
        b"later worktree bytes\n"
    );
}

fn run_export(
    directory: &std::path::Path,
    capture_id: &str,
    output: &std::path::Path,
) -> (std::process::Output, serde_json::Value) {
    let process = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "export", capture_id, "--output"])
        .arg(output)
        .arg("--json")
        .current_dir(directory)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    let json = serde_json::from_slice(&process.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&process.stdout),
            String::from_utf8_lossy(&process.stderr)
        )
    });
    (process, json)
}

#[test]
fn export_reads_all_changes_beyond_the_receipt_limit_and_deduplicates_blobs() {
    let dir = repository_fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "many").unwrap();
    let tree = repo.put_manifest(Vec::new()).unwrap();
    let head = repo
        .put_commit(
            "many".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("many".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("many").unwrap();
    repo.set_branch_head("many", &head).unwrap();
    repo.set_head(&head).unwrap();
    drop(repo);
    for index in 0..80 {
        std::fs::write(dir.path().join(format!("file-{index:03}.txt")), b"shared\n").unwrap();
    }
    let capture_output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "capture", "--json"])
        .current_dir(dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(capture_output.status.success());
    let capture: serde_json::Value = serde_json::from_slice(&capture_output.stdout).unwrap();
    assert_eq!(
        capture["change_set"]["changes"].as_array().unwrap().len(),
        64
    );
    assert_eq!(capture["change_set"]["changes_omitted"], 16);

    let output_path = dir.path().join("many.oakchange");
    let (process, receipt) = run_export(
        dir.path(),
        capture["capture_id"].as_str().unwrap(),
        &output_path,
    );
    assert!(process.status.success(), "{receipt}");
    assert_eq!(receipt["blob_count"], 1);
    assert_eq!(receipt["logical_bytes"], 7);
    let mut archive = zip::ZipArchive::new(std::fs::File::open(output_path).unwrap()).unwrap();
    assert_eq!(archive.len(), 2);
    let manifest: serde_json::Value = {
        let mut bytes = Vec::new();
        archive
            .by_name("manifest.json")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    assert_eq!(
        manifest["change_set"]["changes"].as_array().unwrap().len(),
        80
    );
    assert_eq!(manifest["payload"]["blob_count"], 1);
}

#[test]
fn existing_output_and_missing_blob_fail_without_replacement_or_receipt() {
    let (dir, capture) = captured_fixture();
    let capture_id = capture["capture_id"].as_str().unwrap();
    let output_path = dir.path().join("existing.oakchange");
    std::fs::write(&output_path, b"sentinel").unwrap();
    let (process, error) = run_export(dir.path(), capture_id, &output_path);
    assert!(!process.status.success(), "{error}");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"sentinel");

    std::fs::remove_file(&output_path).unwrap();
    let missing_hash = capture["change_set"]["changes"][0]["after"]["blob_hash"]
        .as_str()
        .unwrap();
    let connection = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    connection
        .execute("DELETE FROM blobs WHERE hash = ?1", [missing_hash])
        .unwrap();
    drop(connection);
    let (process, error) = run_export(dir.path(), capture_id, &output_path);
    assert!(!process.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "incomplete_blob_data");
    assert!(!output_path.exists());
}

#[test]
fn absent_capture_schema_has_upgrade_and_recapture_guidance_without_migration() {
    let dir = repository_fixture();
    let db_path = dir.path().join(".oak/oak.db");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE change_captures;
             DROP TABLE change_sets;
             DELETE FROM schema_migrations WHERE version = '0014_change_captures';",
        )
        .unwrap();
    drop(connection);
    let output_path = dir.path().join("old.oakchange");
    let (process, error) = run_export(
        dir.path(),
        "capture-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &output_path,
    );
    assert!(!process.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "invalid_argument");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("predates durable change captures"));
    assert!(message.contains("oak change capture --json"));
    assert!(!output_path.exists());
    let connection =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('change_sets', 'change_captures')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn partial_current_capture_schema_is_refused_as_inconsistent_not_old() {
    let dir = repository_fixture();
    let db_path = dir.path().join(".oak/oak.db");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch("DROP TABLE change_captures;")
        .unwrap();
    drop(connection);
    let output_path = dir.path().join("partial.oakchange");
    let (process, error) = run_export(
        dir.path(),
        "capture-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &output_path,
    );
    assert!(!process.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "change_capture_integrity");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("incomplete or inconsistent"));
    assert!(!message.contains("oak change capture"));
    assert!(!output_path.exists());
    let connection =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let remaining: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'change_sets'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1);
}

#[test]
fn corrupt_logical_bytes_abort_before_publication() {
    let (dir, capture) = captured_fixture();
    let capture_id = capture["capture_id"].as_str().unwrap();
    let hash = capture["change_set"]["changes"][0]["after"]["blob_hash"]
        .as_str()
        .unwrap();
    let connection = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    connection
        .execute(
            "UPDATE blobs SET content = ?1, size = ?2, codec = 0 WHERE hash = ?3",
            rusqlite::params![b"wrong!".as_slice(), 6_i64, hash],
        )
        .unwrap();
    drop(connection);
    let output_path = dir.path().join("corrupt.oakchange");
    let (process, error) = run_export(dir.path(), capture_id, &output_path);
    assert!(!process.status.success(), "{error}");
    assert_eq!(error["error"]["code"], "captured_blob_integrity");
    assert!(!output_path.exists());
    assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".tmp-")));
}

#[test]
fn canonical_empty_blob_can_be_reconstructed_and_relative_output_is_private() {
    let dir = repository_fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "empty").unwrap();
    let tree = repo.put_manifest(Vec::new()).unwrap();
    let head = repo
        .put_commit(
            "empty".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("empty".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("empty").unwrap();
    repo.set_branch_head("empty", &head).unwrap();
    repo.set_head(&head).unwrap();
    drop(repo);
    std::fs::File::create(dir.path().join("zero.txt")).unwrap();
    let capture_output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["change", "capture", "--json"])
        .current_dir(dir.path())
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(capture_output.status.success());
    let capture: serde_json::Value = serde_json::from_slice(&capture_output.stdout).unwrap();
    let empty_hash = oak_core::hash_bytes(&[]);
    let connection = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    connection
        .execute("DELETE FROM blobs WHERE hash = ?1", [empty_hash.as_str()])
        .unwrap();
    drop(connection);

    let relative = std::path::Path::new("empty.oakchange");
    let (process, receipt) = run_export(
        dir.path(),
        capture["capture_id"].as_str().unwrap(),
        relative,
    );
    assert!(process.status.success(), "{receipt}");
    assert_eq!(receipt["output"], "empty.oakchange");
    assert_eq!(receipt["blob_count"], 1);
    assert_eq!(receipt["logical_bytes"], 0);
    let output_path = dir.path().join(relative);
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&output_path).unwrap()).unwrap();
    assert_eq!(
        archive
            .by_name(&format!("blobs/{empty_hash}"))
            .unwrap()
            .size(),
        0
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(output_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
