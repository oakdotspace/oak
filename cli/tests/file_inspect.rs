use oak_core::{Branch, FileMode, ManifestEntry, Repository, SqliteRepository};
use std::process::Command;

fn fixture(content: &[u8], mode: FileMode) -> (tempfile::TempDir, String, String) {
    fixture_path(content, mode, "dir/file")
}

fn fixture_path(content: &[u8], mode: FileMode, path: &str) -> (tempfile::TempDir, String, String) {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let blob = repo.put_blob(content.to_vec()).unwrap();
    let tree = repo
        .put_manifest(vec![ManifestEntry {
            path: path.into(),
            blob_hash: blob.clone(),
            mode,
        }])
        .unwrap();
    let head = repo
        .put_commit(
            "main".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_head(&head).unwrap();
    (dir, head.to_string(), blob.to_string())
}

fn inspect(
    dir: &std::path::Path,
    at: &str,
    path: &str,
    budget: Option<&str>,
) -> (std::process::Output, serde_json::Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oak"));
    cmd.current_dir(dir)
        .args(["file", "inspect", "--at", at, "--json", path]);
    if let Some(budget) = budget {
        cmd.args(["--max-bytes", budget]);
    }
    let output = cmd.output().unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, value)
}

#[test]
fn pinned_inspection_reports_logical_content_without_reading_checkout() {
    let (dir, head, blob) = fixture(b"recorded bytes\n", FileMode::Regular);
    std::fs::create_dir(dir.path().join("dir")).unwrap();
    std::fs::write(dir.path().join("dir/file"), b"unrelated working tree").unwrap();
    let (out, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert!(out.status.success(), "{value}");
    assert_eq!(value["evidence"]["status"], "verified");
    assert_eq!(value["evidence"]["commit"], head);
    assert_eq!(value["evidence"]["blob"], blob);
    assert_eq!(value["evidence"]["verified_size"], 15);
    assert_eq!(
        value["evidence"]["commit_verification"],
        "verified_v1_header"
    );
    assert_eq!(
        value["evidence"]["proof_scope"],
        serde_json::json!({
            "source": "local_sqlite_snapshot",
            "exclusions": ["commit_files", "ancestor_closure", "remote_durability"]
        })
    );
    assert!(value.get("content").is_none());
}

#[test]
fn head_uses_the_current_branch_effective_head_inside_the_snapshot() {
    let (dir, legacy_head, _) = fixture(b"old bytes", FileMode::Regular);
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let new_blob = repo.put_blob(b"current bytes".to_vec()).unwrap();
    let new_tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "dir/file".into(),
            blob_hash: new_blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let current_head = repo
        .put_commit(
            "main".into(),
            Some(oak_core::Hash(legacy_head)),
            None,
            new_tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("main", &current_head).unwrap();
    repo.set_current_branch("topic").unwrap();
    drop(repo);

    let hash = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .arg("hash")
        .output()
        .unwrap();
    assert!(hash.status.success());
    assert_eq!(
        String::from_utf8(hash.stdout).unwrap().trim(),
        current_head.0
    );

    let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert!(output.status.success(), "{value}");
    assert_eq!(value["evidence"]["commit"], current_head.0);
    assert_eq!(value["evidence"]["blob"], new_blob.0);
}

#[test]
fn compressed_binary_empty_and_symlink_objects_are_logical_bytes() {
    for (bytes, mode) in [
        (vec![b'x'; 200_000], FileMode::Regular),
        (vec![0, 255, 3, 0, 7], FileMode::Executable),
        (Vec::new(), FileMode::Regular),
        (b"../../outside".to_vec(), FileMode::Symlink),
    ] {
        let (dir, head, blob) = fixture(&bytes, mode);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        if bytes.len() > 100_000 {
            assert_eq!(
                conn.query_row("SELECT codec FROM blobs WHERE hash=?1", [&blob], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        if bytes.is_empty() {
            conn.execute("DELETE FROM blobs WHERE hash=?1", [&blob])
                .unwrap();
        }
        drop(conn);
        let (output, value) = inspect(dir.path(), &head, "dir/file", None);
        assert!(output.status.success(), "{value}");
        assert_eq!(value["evidence"]["verified_size"], bytes.len());
        assert_eq!(value["evidence"]["blob"], blob);
        assert_eq!(
            value["evidence"]["mode"],
            serde_json::to_value(mode).unwrap()
        );
    }
}

#[test]
fn absence_corruption_unknown_codec_and_budgets_are_distinct() {
    for expected in [
        "path_missing",
        "object_missing",
        "corrupt",
        "budget_exceeded",
        "unsupported",
    ] {
        let (dir, _, blob) = fixture(&vec![b'a'; 100_000], FileMode::Regular);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        match expected {
            "object_missing" => {
                conn.execute("DELETE FROM blobs WHERE hash=?1", [&blob])
                    .unwrap();
            }
            "corrupt" => {
                conn.execute("UPDATE blobs SET size=size+1 WHERE hash=?1", [&blob])
                    .unwrap();
            }
            "unsupported" => {
                conn.execute("UPDATE blobs SET codec=99 WHERE hash=?1", [&blob])
                    .unwrap();
            }
            _ => {}
        }
        drop(conn);
        let path = if expected == "path_missing" {
            "dir/absent"
        } else {
            "dir/file"
        };
        let budget = (expected == "budget_exceeded").then_some("10");
        let (output, value) = inspect(dir.path(), "HEAD", path, budget);
        assert_eq!(output.status.code(), Some(1), "{value}");
        assert_eq!(value["evidence"]["status"], expected);
        assert!(value["evidence"]["verified_size"].is_null());
    }
}

#[test]
fn malformed_head_is_corruption_not_absent_history() {
    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "UPDATE metadata SET value='not-a-hash' WHERE key='head'",
        [],
    )
    .unwrap();
    drop(conn);
    let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert!(!output.status.success());
    assert_eq!(value["evidence"]["status"], "corrupt");
}

#[test]
fn only_absent_or_explicit_v1_hash_format_can_be_verified() {
    let (dir, head, _) = fixture(b"payload", FileMode::Regular);

    let (output, value) = inspect(dir.path(), &head, "dir/file", None);
    assert!(output.status.success(), "absent defaults to v1: {value}");

    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES('hash_format','v1')",
        [],
    )
    .unwrap();
    drop(conn);
    let (output, value) = inspect(dir.path(), &head, "dir/file", None);
    assert!(output.status.success(), "explicit v1: {value}");

    for format in ["v2", "future"] {
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute(
            "UPDATE metadata SET value=?1 WHERE key='hash_format'",
            [format],
        )
        .unwrap();
        drop(conn);
        let (output, value) = inspect(dir.path(), &head, "dir/file", None);
        assert_eq!(output.status.code(), Some(1), "{format}: {value}");
        assert_eq!(value["evidence"]["status"], "unsupported", "{format}");
    }
}

#[test]
fn object_hash_and_commit_header_tampering_never_verify() {
    for object in ["blob", "tree", "commit", "missing_tree"] {
        let (dir, _, blob) = fixture(b"payload", FileMode::Regular);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        match object {
            "blob" => {
                conn.execute(
                    "UPDATE blobs SET content=?1,codec=0 WHERE hash=?2",
                    rusqlite::params![b"changed".as_slice(), blob],
                )
                .unwrap();
            }
            "tree" => {
                conn.execute("UPDATE trees SET content=?1 WHERE hash=(SELECT manifest_hash FROM commits LIMIT 1)", [zstd::encode_all(b"invalid canonical tree".as_slice(), 3).unwrap()]).unwrap();
            }
            "commit" => {
                conn.execute("UPDATE commits SET author='tampered'", [])
                    .unwrap();
            }
            "missing_tree" => {
                conn.execute(
                    "DELETE FROM trees WHERE hash=(SELECT manifest_hash FROM commits LIMIT 1)",
                    [],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);
        let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
        assert!(!output.status.success(), "{object}: {value}");
        assert_eq!(
            value["evidence"]["status"],
            if object == "missing_tree" {
                "object_missing"
            } else {
                "corrupt"
            }
        );
    }
}

#[test]
fn malformed_sql_value_types_are_corruption_not_storage_unavailability() {
    for object in ["hash_format", "commit_header", "blob_codec"] {
        let (dir, head, blob) = fixture(b"payload", FileMode::Regular);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        match object {
            "hash_format" => {
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES('hash_format',x'ff')",
                    [],
                )
                .unwrap();
            }
            "commit_header" => {
                conn.execute("UPDATE commits SET author=x'ff' WHERE hash=?1", [&head])
                    .unwrap();
            }
            "blob_codec" => {
                conn.execute("UPDATE blobs SET codec='invalid' WHERE hash=?1", [&blob])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);

        let (output, value) = inspect(dir.path(), &head, "dir/file", None);
        assert_eq!(output.status.code(), Some(1), "{object}: {value}");
        assert_eq!(value["evidence"]["status"], "corrupt", "{object}");
    }
}

#[test]
fn oversized_reference_fields_are_budget_exceeded_without_allocation() {
    for field in [
        "hash_format",
        "current_branch",
        "legacy_head",
        "branch_head",
        "parent_branch",
    ] {
        let (dir, head, _) = fixture(b"payload", FileMode::Regular);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        let oversized = "x".repeat(64 * 1024 + 1);
        match field {
            "hash_format" => {
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES('hash_format',?1)",
                    [&oversized],
                )
                .unwrap();
            }
            "current_branch" => {
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES('current_branch',?1)",
                    [&oversized],
                )
                .unwrap();
            }
            "legacy_head" => {
                conn.execute(
                    "UPDATE metadata SET value=?1 WHERE key='head'",
                    [&oversized],
                )
                .unwrap();
            }
            "branch_head" => {
                conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES('current_branch','topic')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO branch_heads(branch_name,head_hash) VALUES('topic',?1)",
                    [&oversized],
                )
                .unwrap();
            }
            "parent_branch" => {
                conn.execute(
                    "INSERT INTO metadata(key,value) VALUES('current_branch','topic')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO branches(name,parent_branch) VALUES('topic',?1)",
                    [&oversized],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);

        let revision = if field == "hash_format" {
            &head
        } else {
            "HEAD"
        };
        let (output, value) = inspect(dir.path(), revision, "dir/file", None);
        assert_eq!(output.status.code(), Some(1), "{field}: {value}");
        assert_eq!(value["evidence"]["status"], "budget_exceeded", "{field}");
    }
}

#[test]
fn head_parent_walk_is_bounded_and_cycles_are_corruption() {
    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES('current_branch','branch-0')",
        [],
    )
    .unwrap();
    for index in 0..129 {
        conn.execute(
            "INSERT INTO branches(name,parent_branch) VALUES(?1,?2)",
            rusqlite::params![format!("branch-{index}"), format!("branch-{}", index + 1)],
        )
        .unwrap();
    }
    drop(conn);
    let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert_eq!(output.status.code(), Some(1), "{value}");
    assert_eq!(value["evidence"]["status"], "budget_exceeded");

    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES('current_branch','left')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO branches(name,parent_branch) VALUES('left','right'),('right','left')",
        [],
    )
    .unwrap();
    drop(conn);
    let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert_eq!(output.status.code(), Some(1), "{value}");
    assert_eq!(value["evidence"]["status"], "corrupt");
}

#[test]
fn read_only_snapshot_pins_head_and_never_changes_refs_or_content() {
    let (dir, original, _) = fixture(b"payload", FileMode::Regular);
    let db_path = dir.path().join(".oak/oak.db");
    let snapshot = SqliteRepository::open_read_only(&db_path).unwrap();
    let writer = SqliteRepository::open(&db_path).unwrap();
    let root = writer
        .get_commit(&oak_core::Hash(original.clone()))
        .unwrap()
        .unwrap()
        .manifest_hash;
    let newer = writer
        .put_commit(
            "main".into(),
            Some(oak_core::Hash(original.clone())),
            None,
            root,
            "writer".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    writer.set_head(&newer).unwrap();
    let evidence = snapshot
        .inspect_pinned_file("HEAD", "dir/file", 100)
        .unwrap();
    assert_eq!(evidence.commit.as_deref(), Some(original.as_str()));
    assert_eq!(writer.get_head().unwrap(), Some(newer));
    drop(snapshot);
    drop(writer);
    let before = std::fs::read(&db_path).unwrap();
    let (output, value) = inspect(dir.path(), &original, "dir/file", None);
    assert!(output.status.success(), "{value}");
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
    assert!(!dir.path().join("dir").exists());
}

#[test]
fn logical_decompression_and_header_budgets_are_inconclusive() {
    for object in ["blob", "tree", "header"] {
        let (dir, _, _) = fixture(&vec![b'x'; 100_000], FileMode::Regular);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        match object {
            "blob" => {
                conn.execute("UPDATE blobs SET size=1", []).unwrap();
            }
            "tree" => {
                conn.execute(
                    "UPDATE trees SET content=?1",
                    [zstd::encode_all(vec![b'x'; 17 * 1024 * 1024].as_slice(), 3).unwrap()],
                )
                .unwrap();
            }
            "header" => {
                conn.execute("UPDATE commits SET message=?1", ["x".repeat(70_000)])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);
        let (output, value) = inspect(dir.path(), "HEAD", "dir/file", Some("100"));
        assert!(!output.status.success(), "{value}");
        assert_eq!(
            value["evidence"]["status"], "budget_exceeded",
            "{object}: {value}"
        );
    }
}

#[test]
fn path_and_revision_errors_do_not_mutate_the_checkout() {
    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let db = dir.path().join(".oak/oak.db");
    let before = std::fs::read(&db).unwrap();
    for path in ["../file", "/file", "dir//file", "dir/./file", "C:/file"] {
        let (output, value) = inspect(dir.path(), "HEAD", path, None);
        assert_eq!(output.status.code(), Some(2), "{path}: {value}");
        assert!(value.get("error").is_some());
    }
    let (output, value) = inspect(dir.path(), "main", "dir/file", None);
    assert_eq!(output.status.code(), Some(2), "{value}");
    assert_eq!(std::fs::read(db).unwrap(), before);
}

#[test]
fn legacy_tree_rows_are_verified_against_the_same_canonical_hash() {
    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    let rows: Vec<(String, Vec<u8>)> = conn
        .prepare("SELECT hash,content FROM trees")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (hash, encoded) in rows {
        let tree = oak_core::Tree::from_canonical_bytes(
            oak_core::Hash(hash.clone()),
            &zstd::decode_all(encoded.as_slice()).unwrap(),
        )
        .unwrap();
        for entry in tree.entries {
            conn.execute(
                "INSERT INTO tree_entries(tree_hash,name,kind,hash,mode) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![
                    hash,
                    entry.name,
                    entry.kind.as_str(),
                    entry.hash.as_str(),
                    "regular"
                ],
            )
            .unwrap();
        }
        conn.execute("UPDATE trees SET content=NULL WHERE hash=?1", [&hash])
            .unwrap();
    }
    drop(conn);
    let (output, value) = inspect(dir.path(), "HEAD", "dir/file", None);
    assert!(output.status.success(), "{value}");
    assert_eq!(value["evidence"]["verified_trees"], 2);
}

#[test]
fn git_backend_and_registered_mount_are_explicitly_unsupported() {
    let git = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(git.path().join(".git")).unwrap();
    let (output, value) = inspect(git.path(), "HEAD", "file", None);
    assert!(!output.status.success());
    assert!(value["error"]["message"].as_str().unwrap().contains("git"));
    assert!(!git.path().join(".git/oak").exists());

    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let state = tempfile::TempDir::new().unwrap();
    oak_cli::commands::mount::state::set_mounts_root(state.path().to_path_buf());
    oak_cli::commands::mount::state::register_mount(
        &dir.path().canonicalize().unwrap(),
        "inspection-test",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .env("OAK_MOUNTS_ROOT", state.path())
        .args(["file", "inspect", "--at", "HEAD", "--json", "dir/file"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains("mount"));
}

#[test]
fn human_inspection_has_no_background_update_side_effects() {
    let (dir, _, _) = fixture(b"payload", FileMode::Regular);
    let home = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("OAK_NO_UPDATE_CHECK")
        .args(["file", "inspect", "--at", "HEAD", "dir/file"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join(".oak/version_check").exists());
}

#[test]
fn tracked_oak_workflows_are_inspectable_without_opening_metadata_paths() {
    let (dir, _, _) = fixture_path(b"on: [push]\n", FileMode::Regular, ".oak/workflows/ci.yml");
    let (output, value) = inspect(dir.path(), "HEAD", ".oak/workflows/ci.yml", None);
    assert!(output.status.success(), "{value}");
    assert_eq!(value["evidence"]["status"], "verified");
    let (output, value) = inspect(dir.path(), "HEAD", ".oak/oak.db", None);
    assert!(!output.status.success());
    assert_eq!(value["evidence"]["status"], "path_missing");
}
