include!("change_capture.rs");

#[test]
fn council_rejects_legacy_tree_with_same_flattened_manifest_wrong_identity() {
    let (dir, _) = fixture();
    let entries = vec![ManifestEntry {
        path: "tracked.txt".into(),
        blob_hash: oak_core::hash_bytes(b"captured bytes\n"),
        mode: FileMode::Regular,
    }];
    let expected = oak_core::build_tree(&entries).unwrap().root_hash;
    let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
    conn.execute("INSERT INTO trees(hash) VALUES (?1)", [expected.as_str()])
        .unwrap();
    conn.execute("INSERT INTO tree_entries(tree_hash,name,kind,hash,mode) VALUES (?1,'tracked.txt','blob',?2,'regular')", rusqlite::params![expected.as_str(), entries[0].blob_hash.as_str()]).unwrap();
    conn.execute("INSERT INTO tree_entries(tree_hash,name,kind,hash,mode) VALUES (?1,'phantom','tree',?2,'tree')", rusqlite::params![expected.as_str(), oak_core::Tree::empty_hash().as_str()]).unwrap();
    drop(conn);
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert_ne!(
        oak_core::hash_bytes(&repo.get_tree(&expected).unwrap().unwrap().canonical_bytes()),
        expected
    );
    assert_eq!(repo.walk_tree(&expected).unwrap().len(), 1);
    drop(repo);
    let (output, receipt) = capture(dir.path(), &[]);
    assert!(
        !output.status.success(),
        "certified corrupt legacy tree: {receipt}"
    );
    assert_eq!(receipt["error"]["code"], "captured_tree_integrity");
}

#[test]
fn council_scoped_deletion_and_new_directory_are_reconstructable() {
    let (dir, _) = fixture();
    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
    std::fs::create_dir(dir.path().join("newdir")).unwrap();
    std::fs::write(dir.path().join("newdir/a"), b"selected new content").unwrap();
    let (output, receipt) = capture(dir.path(), &["./tracked.txt", "newdir/../newdir"]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["change_set"]["change_count"], 2);
    assert_eq!(
        receipt["scope"]["paths"],
        serde_json::json!(["newdir", "tracked.txt"])
    );
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let hash =
        oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap()).unwrap();
    let result = repo.get_manifest(&hash).unwrap().unwrap();
    assert!(result.get("tracked.txt").is_none());
    assert_eq!(
        repo.get_blob(&result.get("newdir/a").unwrap().blob_hash)
            .unwrap()
            .unwrap()
            .content,
        b"selected new content"
    );
}

#[cfg(unix)]
#[test]
fn council_rejects_ambiguous_backslash_filename_collision() {
    let (dir, _) = fixture();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::write(dir.path().join("a/b"), b"one").unwrap();
    std::fs::write(dir.path().join("a\\b"), b"two").unwrap();
    let (output, receipt) = capture(dir.path(), &[]);
    assert!(
        !output.status.success(),
        "silently discarded one logical file: {receipt}"
    );
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    for bytes in [b"one".as_slice(), b"two", b"captured bytes\n"] {
        assert!(!repo.has_blob(&oak_core::hash_bytes(bytes)).unwrap());
    }
}

#[test]
fn council_ignored_tracked_deletion_matches_existing_oak_semantics() {
    let (dir, _) = fixture();
    std::fs::write(dir.path().join(".oakignore"), b"tracked.txt\n").unwrap();
    let (output, receipt) = capture(dir.path(), &["tracked.txt"]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["change_set"]["change_count"], 1);
    assert!(receipt["change_set"]["changes"][0]["after"].is_null());
    assert_eq!(
        std::fs::read(dir.path().join("tracked.txt")).unwrap(),
        b"captured bytes\n"
    );
}

#[cfg(unix)]
#[test]
fn council_mode_only_change_and_authority_independent_identity() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _) = fixture();
    std::fs::write(dir.path().join("tracked.txt"), b"base bytes\n").unwrap();
    std::fs::set_permissions(
        dir.path().join("tracked.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (output, first) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{first}");
    let change = &first["change_set"]["changes"][0];
    assert_eq!(change["before"]["blob_hash"], change["after"]["blob_hash"]);
    assert_eq!(change["after"]["mode"], "Executable");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "another-owner")
        .unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://another-origin.invalid")
        .unwrap();
    drop(repo);
    let (output, second) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{second}");
    assert_eq!(first["change_set"]["id"], second["change_set"]["id"]);
    assert_ne!(first["repository"], second["repository"]);
}

#[cfg(unix)]
#[test]
fn council_lone_backslash_cannot_alias_an_unselected_physical_path() {
    let (dir, _) = fixture();
    let bytes = b"physical backslash unique content";
    std::fs::write(dir.path().join("a\\b"), bytes).unwrap();
    let (output, receipt) = capture(dir.path(), &["a/b"]);
    if output.status.success() {
        assert_eq!(
            receipt["change_set"]["change_count"], 0,
            "captured different physical path: {receipt}"
        );
    }
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert!(
        !repo.has_blob(&oak_core::hash_bytes(bytes)).unwrap(),
        "persisted unselected physical path"
    );
}

#[test]
fn council_restored_unavailable_file_is_not_silently_omitted() {
    for marker in [MetadataKey::RestrictedBlobs, MetadataKey::KnownLostBlobs] {
        let (dir, _) = fixture();
        let hash = oak_core::hash_bytes(b"base bytes\n");
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(marker, hash.as_str()).unwrap();
        drop(repo);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute("DELETE FROM blobs WHERE hash=?1", [hash.as_str()])
            .unwrap();
        drop(conn);
        let (output, receipt) = capture(dir.path(), &["tracked.txt"]);
        assert!(
            !output.status.success(),
            "present excluded bytes silently acknowledged: {receipt}"
        );
        assert_eq!(receipt["error"]["code"], "capture_scope_excluded");
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        assert_eq!(
            repo.get_metadata(marker).unwrap().as_deref(),
            Some(hash.as_str())
        );
        assert!(!repo
            .has_blob(&oak_core::hash_bytes(b"captured bytes\n"))
            .unwrap());
        assert_eq!(
            std::fs::read(dir.path().join("tracked.txt")).unwrap(),
            b"captured bytes\n"
        );
    }
}

#[cfg(unix)]
#[test]
fn council_rejects_backslash_input_before_normalization_or_root_selection() {
    for paths in [vec!["a\\b"], vec![".", "a\\b"], vec!["a\\b/.."]] {
        let (dir, _) = fixture();
        let (output, receipt) = capture(dir.path(), &paths);
        assert!(
            !output.status.success(),
            "accepted ambiguous input: {receipt}"
        );
        assert_eq!(receipt["error"]["code"], "invalid_argument");
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        assert!(!repo
            .has_blob(&oak_core::hash_bytes(b"captured bytes\n"))
            .unwrap());
    }
}

#[cfg(windows)]
#[test]
fn council_windows_separators_remain_valid_capture_inputs() {
    let (dir, _) = fixture();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested/a"), b"windows separator bytes").unwrap();
    let (output, receipt) = capture(dir.path(), &["nested\\a"]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["change_set"]["changes"][0]["path"], "nested/a");
}

#[test]
fn council_verifies_legacy_subtree_identity_and_accepts_valid_legacy_nodes() {
    for corrupt in [false, true] {
        let (dir, _) = fixture();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/a"), b"nested capture bytes").unwrap();
        let blob = oak_core::hash_bytes(b"nested capture bytes");
        let built = oak_core::build_tree(&[ManifestEntry {
            path: "nested/a".into(),
            blob_hash: blob.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
        let subtree = built
            .trees
            .iter()
            .find(|tree| tree.hash != built.root_hash)
            .unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute(
            "INSERT INTO trees(hash) VALUES (?1)",
            [subtree.hash.as_str()],
        )
        .unwrap();
        conn.execute("INSERT INTO tree_entries(tree_hash,name,kind,hash,mode) VALUES (?1,'a','blob',?2,'regular')", rusqlite::params![subtree.hash.as_str(), blob.as_str()]).unwrap();
        if corrupt {
            conn.execute("INSERT INTO tree_entries(tree_hash,name,kind,hash,mode) VALUES (?1,'phantom','tree',?2,'tree')", rusqlite::params![subtree.hash.as_str(), oak_core::Tree::empty_hash().as_str()]).unwrap();
        }
        drop(conn);
        let (output, receipt) = capture(dir.path(), &["nested"]);
        assert_eq!(output.status.success(), !corrupt, "{receipt}");
        if corrupt {
            assert_eq!(receipt["error"]["code"], "captured_tree_integrity");
        }
    }
}

#[test]
fn council_whole_capture_reports_present_unavailable_exclusions_in_both_formats() {
    for (marker, label) in [
        (MetadataKey::RestrictedBlobs, "restricted"),
        (MetadataKey::KnownLostBlobs, "known-loss"),
    ] {
        let (dir, head) = fixture();
        let hash = oak_core::hash_bytes(b"base bytes\n");
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(marker, hash.as_str()).unwrap();
        drop(repo);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute("DELETE FROM blobs WHERE hash=?1", [hash.as_str()])
            .unwrap();
        drop(conn);
        let (output, receipt) = capture(dir.path(), &[]);
        assert!(output.status.success(), "{receipt}");
        assert_eq!(
            receipt["scope"]["effective"]["unavailable_base_paths_present"],
            1
        );
        assert_eq!(receipt["change_set"]["change_count"], 0);
        let warnings = receipt["warnings"].as_array().unwrap();
        assert!(warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains(label)));
        assert!(warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("NOT captured")));
        let output = Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(["change", "capture"])
            .current_dir(dir.path())
            .env("OAK_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        let human = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            human.contains(label) && human.contains("NOT captured"),
            "{human}"
        );
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        assert_eq!(repo.get_branch_head("topic").unwrap(), Some(head));
        assert_eq!(
            repo.get_metadata(marker).unwrap().as_deref(),
            Some(hash.as_str())
        );
        assert!(!repo
            .has_blob(&oak_core::hash_bytes(b"captured bytes\n"))
            .unwrap());
    }
}
