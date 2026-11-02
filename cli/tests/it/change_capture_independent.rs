include!("change_capture.rs");

#[cfg(unix)]
#[test]
fn independent_empty_binary_large_executable_and_symlink_bytes() {
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
    let result = repo
        .get_manifest(
            &oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap()
        .unwrap();
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
fn independent_read_error_cannot_become_empty_capture() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _) = fixture();
    let path = dir.path().join("tracked.txt");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (output, error) = capture_without_read_privilege(dir.path());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!output.status.success(), "read error certified as capture");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{error}"
    );
}

#[test]
fn independent_capture_never_certifies_wrong_existing_blob_bytes() {
    let (dir, _) = fixture();
    let captured_hash = oak_core::hash_bytes(b"captured bytes\n");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.store_blob(&oak_core::Blob {
        hash: captured_hash.clone(),
        content: b"wrong bytes".to_vec(),
        size: 11,
    })
    .unwrap();
    drop(repo);
    let (output, _) = capture(dir.path(), &[]);
    if output.status.success() {
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        assert_eq!(
            repo.get_blob(&captured_hash).unwrap().unwrap().content,
            b"captured bytes\n",
            "receipt certified bytes that were not persisted"
        );
    }
}

#[test]
fn independent_rejects_changes_outside_declared_scope() {
    let result = oak_core::Manifest::new(vec![ManifestEntry {
        path: "b".into(),
        blob_hash: oak_core::hash_bytes(b"b"),
        mode: FileMode::Regular,
    }]);
    let rejected: Vec<_> = [vec![], vec!["a".into()]]
        .into_iter()
        .map(|paths| {
            oak_core::CanonicalChangeSetV1::from_manifests(
                &oak_core::Manifest::empty(),
                &result,
                oak_core::ChangeScopeV1::Paths { paths },
            )
            .is_err()
        })
        .collect();
    assert_eq!(
        rejected,
        vec![true, true],
        "certified changes outside declared scope"
    );
}

#[test]
fn independent_unavailable_paths_survive_identical_new_content() {
    let mut preserved = Vec::new();
    for marker in [MetadataKey::RestrictedBlobs, MetadataKey::KnownLostBlobs] {
        let (dir, head) = fixture();
        std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        let hash = oak_core::hash_bytes(b"base bytes\n");
        repo.set_metadata(marker, hash.as_str()).unwrap();
        drop(repo);
        let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
        conn.execute("DELETE FROM blobs WHERE hash=?1", [hash.as_str()])
            .unwrap();
        drop(conn);
        std::fs::write(dir.path().join("new.txt"), b"base bytes\n").unwrap();
        let (output, receipt) = capture(dir.path(), &[]);
        assert!(output.status.success(), "{receipt}");
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        assert_eq!(repo.get_branch_head("topic").unwrap(), Some(head));
        let result = repo
            .get_manifest(
                &oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        preserved.push(result.get("tracked.txt").is_some());
    }
    assert_eq!(
        preserved,
        vec![true, true],
        "phantom deletions of restricted and known-lost base entries"
    );
}

#[test]
fn independent_sparse_old_path_survives_identical_new_content() {
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
    let result = repo
        .get_manifest(
            &oak_core::Hash::from_hex(receipt["change_set"]["result_tree"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(
        result.get("tracked.txt").is_some(),
        "phantom deletion of out-of-cone base entry"
    );
}

#[test]
fn independent_rename_does_not_delete_unselected_old_path() {
    let (dir, _) = fixture();
    std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();
    std::fs::write(dir.path().join("new.txt"), b"base bytes\n").unwrap();
    let (output, receipt) = capture(dir.path(), &["new.txt"]);
    assert!(output.status.success(), "{receipt}");
    let changed_paths: Vec<_> = receipt["change_set"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(changed_paths, vec!["new.txt"]);
}

#[test]
fn independent_rejects_invalid_blob_identity() {
    let result = oak_core::Manifest::new(vec![ManifestEntry {
        path: "a".into(),
        blob_hash: oak_core::Hash("not-a-hash".into()),
        mode: FileMode::Regular,
    }]);
    assert!(
        oak_core::CanonicalChangeSetV1::from_manifests(
            &oak_core::Manifest::empty(),
            &result,
            oak_core::ChangeScopeV1::Full
        )
        .is_err(),
        "certified an invalid blob identity"
    );
}

#[cfg(unix)]
#[test]
fn independent_scoped_symlink_captures_link_not_target() {
    let (dir, _) = fixture();
    std::os::unix::fs::symlink("tracked.txt", dir.path().join("link")).unwrap();
    let (output, receipt) = capture(dir.path(), &["link"]);
    assert!(output.status.success(), "{receipt}");
    assert_eq!(receipt["scope"]["paths"][0], "link");
    assert_eq!(receipt["change_set"]["changes"][0]["path"], "link");
}

#[cfg(unix)]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS fixture filesystem rejects non-UTF8 filename bytes before Oak runs"
)]
fn independent_rejects_unrepresentable_filename() {
    use std::os::unix::ffi::OsStringExt;
    let (dir, _) = fixture();
    let filename = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
    std::fs::write(dir.path().join(filename), b"unrepresentable path bytes").unwrap();
    let (output, receipt) = capture(dir.path(), &[]);
    assert!(
        !output.status.success(),
        "certified lossy path {}",
        receipt["change_set"]["changes"]
    );
}

#[test]
fn independent_scoped_capture_does_not_persist_unselected_bytes() {
    let (dir, _) = fixture();
    let bytes = b"unselected unique bytes";
    std::fs::write(dir.path().join("unselected.txt"), bytes).unwrap();
    let (output, receipt) = capture(dir.path(), &["tracked.txt"]);
    assert!(output.status.success(), "{receipt}");
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    assert!(
        !repo.has_blob(&oak_core::hash_bytes(bytes)).unwrap(),
        "persisted outside requested scope"
    );
}

#[test]
fn independent_origin_does_not_include_fake_credentials() {
    let (dir, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    repo.set_metadata(
        MetadataKey::RemoteUrl,
        "https://fake-user:fake-password@example.invalid/path?token=fake-token",
    )
    .unwrap();
    drop(repo);
    let (output, receipt) = capture(dir.path(), &[]);
    assert!(output.status.success());
    let origin = receipt["repository"]["origin"].as_str().unwrap();
    assert!(
        !origin.contains("fake-password") && !origin.contains("fake-token"),
        "origin leaked fixture credential material"
    );
}
