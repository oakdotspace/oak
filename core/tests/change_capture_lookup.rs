use oak_core::{
    Branch, CanonicalChangeSetV1, ChangeScopeV1, FileMode, ManifestEntry, MetadataKey, OakError,
    Repository, SqliteRepository, StatCacheEntry,
};

const CAPTURE_A: &str = "capture-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CAPTURE_B: &str = "capture-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture() -> (tempfile::TempDir, oak_core::Hash, CanonicalChangeSetV1) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widgets").unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://oak.space/team")
        .unwrap();
    let before = repo.put_blob(b"before\n".to_vec()).unwrap();
    let after = repo.put_blob(b"after\n".to_vec()).unwrap();
    let base_tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: before,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let result_tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: after,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let head = repo
        .put_commit(
            "topic".to_string(),
            None,
            None,
            base_tree.clone(),
            "tester".to_string(),
            None,
            chrono::Utc::now(),
            Vec::new(),
        )
        .unwrap();
    repo.store_branch(&Branch::new(
        "topic".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("topic").unwrap();
    repo.set_branch_head("topic", &head).unwrap();
    repo.set_head(&head).unwrap();
    let base = repo.get_manifest(&base_tree).unwrap().unwrap();
    let result = repo.get_manifest(&result_tree).unwrap().unwrap();
    let change_set =
        CanonicalChangeSetV1::from_manifests(&base, &result, ChangeScopeV1::Full).unwrap();
    (dir, head, change_set)
}

#[test]
fn lookup_rejects_corrupt_descriptor_and_secret_origin_without_repair_or_disclosure() {
    let (dir, head, change_set) = fixture();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    repo.store_change_capture(
        CAPTURE_A,
        &change_set,
        "acme",
        "widgets",
        Some("https://oak.space/team"),
        "topic",
        Some(&head),
    )
    .unwrap();
    drop(repo);

    let secret = "reporter-secret-password";
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_captures SET origin = ?1 WHERE capture_id = ?2",
        [
            format!("https://user:{secret}@example.invalid"),
            CAPTURE_A.to_string(),
        ],
    )
    .unwrap();
    drop(conn);
    let repo = SqliteRepository::open(&db).unwrap();
    let error = repo.get_change_capture(CAPTURE_A).unwrap_err();
    assert!(matches!(error, OakError::ChangeCaptureIntegrity { .. }));
    assert!(!error.to_string().contains(secret));
    drop(repo);

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_captures SET origin = 'https://oak.space/team' WHERE capture_id = ?1",
        [CAPTURE_A],
    )
    .unwrap();
    let corrupt = r#"{"changes":[{"path":"secret-row-value"}]}"#;
    conn.execute(
        "UPDATE change_sets SET descriptor_json = ?1 WHERE change_id = ?2",
        [corrupt, change_set.id().as_str()],
    )
    .unwrap();
    drop(conn);

    let repo = SqliteRepository::open(&db).unwrap();
    let error = repo.get_change_capture(CAPTURE_A).unwrap_err();
    assert!(matches!(error, OakError::ChangeCaptureIntegrity { .. }));
    assert!(!error.to_string().contains("secret-row-value"));
    drop(repo);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let still_corrupt: String = conn
        .query_row(
            "SELECT descriptor_json FROM change_sets WHERE change_id = ?1",
            [change_set.id().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        still_corrupt, corrupt,
        "lookup must never repair corrupt rows"
    );
}

#[test]
fn unknown_capture_is_typed_not_found() {
    let (dir, _, _) = fixture();
    let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
    let error = repo.get_change_capture(CAPTURE_B).unwrap_err();
    assert!(matches!(error, OakError::ChangeCaptureNotFound { .. }));
}

#[test]
fn lookup_rejects_noncanonical_columns_and_dangling_occurrences() {
    let (dir, head, change_set) = fixture();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    repo.store_change_capture(
        CAPTURE_A,
        &change_set,
        "acme",
        "widgets",
        Some("https://oak.space/team"),
        "topic",
        Some(&head),
    )
    .unwrap();
    drop(repo);

    let assert_integrity = || {
        let repo = SqliteRepository::open(&db).unwrap();
        assert!(matches!(
            repo.get_change_capture(CAPTURE_A).unwrap_err(),
            OakError::ChangeCaptureIntegrity { .. }
        ));
    };

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_sets SET scope_kind = 'paths', scope_paths_json = '[\"file.txt\",\"file.txt\"]' WHERE change_id = ?1",
        [change_set.id().as_str()],
    )
    .unwrap();
    drop(conn);
    assert_integrity();

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_sets SET scope_kind = 'full', scope_paths_json = '[]', base_tree_hash = ?1 WHERE change_id = ?2",
        ["a".repeat(40), change_set.id().to_string()],
    )
    .unwrap();
    drop(conn);
    assert_integrity();

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_captures SET format_version = 2 WHERE capture_id = ?1",
        [CAPTURE_A],
    )
    .unwrap();
    drop(conn);
    assert_integrity();

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute(
        "UPDATE change_captures SET format_version = 1 WHERE capture_id = ?1",
        [CAPTURE_A],
    )
    .unwrap();
    conn.execute(
        "UPDATE change_sets SET base_tree_hash = ?1 WHERE change_id = ?2",
        [change_set.base_tree().as_str(), change_set.id().as_str()],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM change_sets WHERE change_id = ?1",
        [change_set.id().as_str()],
    )
    .unwrap();
    drop(conn);
    assert_integrity();
}

#[test]
fn publication_is_atomic_and_rechecks_the_current_branch() {
    let (dir, head, first_change_set) = fixture();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    repo.store_change_capture(
        CAPTURE_A,
        &first_change_set,
        "acme",
        "widgets",
        Some("https://oak.space/team"),
        "topic",
        Some(&head),
    )
    .unwrap();

    let different_blob = repo.put_blob(b"different\n".to_vec()).unwrap();
    let different_tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "file.txt".to_string(),
            blob_hash: different_blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let base_tree = first_change_set.base_tree().clone();
    let base = repo.get_manifest(&base_tree).unwrap().unwrap();
    let result = repo.get_manifest(&different_tree).unwrap().unwrap();
    let second_change_set =
        CanonicalChangeSetV1::from_manifests(&base, &result, ChangeScopeV1::Full).unwrap();

    let duplicate_error = repo
        .store_change_capture(
            CAPTURE_A,
            &second_change_set,
            "acme",
            "widgets",
            Some("https://oak.space/team"),
            "topic",
            Some(&head),
        )
        .unwrap_err();
    assert!(matches!(
        duplicate_error,
        OakError::ChangeCaptureIntegrity { .. }
    ));
    assert_eq!(
        repo.get_change_capture(CAPTURE_A).unwrap().change_set,
        first_change_set
    );
    drop(repo);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let second_descriptor_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM change_sets WHERE change_id = ?1",
            [second_change_set.id().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        second_descriptor_count, 0,
        "a failed occurrence insert must roll back its new descriptor"
    );
    drop(conn);

    let repo = SqliteRepository::open(&db).unwrap();
    repo.store_branch(&Branch::new(
        "other".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_branch_head("other", &head).unwrap();
    repo.set_current_branch("other").unwrap();
    let raced = repo
        .store_change_capture(
            CAPTURE_B,
            &second_change_set,
            "acme",
            "widgets",
            Some("https://oak.space/team"),
            "topic",
            Some(&head),
        )
        .unwrap_err();
    assert!(matches!(raced, OakError::WorktreeCaptureRaced { .. }));
    assert!(matches!(
        repo.get_change_capture(CAPTURE_B).unwrap_err(),
        OakError::ChangeCaptureNotFound { .. }
    ));
}

#[test]
fn descriptor_base_is_bound_to_the_recorded_commit_or_headless_empty_tree() {
    let (dir, head, change_set) = fixture();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    let unrelated_base = repo
        .get_manifest(change_set.result_tree())
        .unwrap()
        .unwrap();
    let unrelated_result = repo.get_manifest(change_set.base_tree()).unwrap().unwrap();
    let wrong = CanonicalChangeSetV1::from_manifests(
        &unrelated_base,
        &unrelated_result,
        ChangeScopeV1::Full,
    )
    .unwrap();
    assert!(matches!(
        repo.store_change_capture(
            CAPTURE_A,
            &wrong,
            "acme",
            "widgets",
            Some("https://oak.space/team"),
            "topic",
            Some(&head),
        )
        .unwrap_err(),
        OakError::ChangeCaptureIntegrity { .. }
    ));
    assert!(matches!(
        repo.get_change_capture(CAPTURE_A).unwrap_err(),
        OakError::ChangeCaptureNotFound { .. }
    ));
    repo.store_change_capture(
        CAPTURE_A,
        &change_set,
        "acme",
        "widgets",
        Some("https://oak.space/team"),
        "topic",
        Some(&head),
    )
    .unwrap();
    let unrelated_commit = repo
        .put_commit(
            "historical".into(),
            None,
            None,
            change_set.result_tree().clone(),
            "tester".into(),
            None,
            chrono::Utc::now(),
            Vec::new(),
        )
        .unwrap();
    drop(repo);

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE change_captures SET base_commit_hash = ?1 WHERE capture_id = ?2",
        [unrelated_commit.as_str(), CAPTURE_A],
    )
    .unwrap();
    drop(conn);
    let repo = SqliteRepository::open(&db).unwrap();
    assert!(matches!(
        repo.get_change_capture(CAPTURE_A).unwrap_err(),
        OakError::ChangeCaptureIntegrity { .. }
    ));
    drop(repo);

    let headless_dir = tempfile::TempDir::new().unwrap();
    let headless_db = headless_dir.path().join("oak.db");
    let headless = SqliteRepository::open(&headless_db).unwrap();
    headless
        .store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    headless.set_current_branch("topic").unwrap();
    let before = headless.put_blob(b"before\n".to_vec()).unwrap();
    let after = headless.put_blob(b"after\n".to_vec()).unwrap();
    let nonempty_base = headless
        .put_manifest(vec![ManifestEntry {
            path: "file.txt".into(),
            blob_hash: before,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let result = headless
        .put_manifest(vec![ManifestEntry {
            path: "file.txt".into(),
            blob_hash: after,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let wrong_headless = CanonicalChangeSetV1::from_manifests(
        &headless.get_manifest(&nonempty_base).unwrap().unwrap(),
        &headless.get_manifest(&result).unwrap().unwrap(),
        ChangeScopeV1::Full,
    )
    .unwrap();
    assert!(matches!(
        headless
            .store_change_capture(
                CAPTURE_B,
                &wrong_headless,
                "acme",
                "widgets",
                None,
                "topic",
                None,
            )
            .unwrap_err(),
        OakError::ChangeCaptureIntegrity { .. }
    ));
    let empty = oak_core::Manifest::empty();
    let valid_headless = CanonicalChangeSetV1::from_manifests(
        &empty,
        &headless.get_manifest(&result).unwrap().unwrap(),
        ChangeScopeV1::Full,
    )
    .unwrap();
    headless
        .store_change_capture(
            CAPTURE_A,
            &valid_headless,
            "acme",
            "widgets",
            None,
            "topic",
            None,
        )
        .unwrap();
    assert_eq!(
        headless.get_change_capture(CAPTURE_A).unwrap().change_set,
        valid_headless
    );
}

#[test]
fn migration_from_0013_preserves_repository_state_and_is_restart_safe() {
    let (dir, head, change_set) = fixture();
    let db = dir.path().join("oak.db");
    let repo = SqliteRepository::open(&db).unwrap();
    let base = repo.get_manifest(change_set.base_tree()).unwrap().unwrap();
    let cached_blob = base.entries[0].blob_hash.clone();
    repo.update_stat_cache(
        &[(
            "file.txt".to_string(),
            StatCacheEntry {
                mtime_ns: 11,
                ctime_ns: 12,
                size: 7,
                blob_hash: cached_blob,
            },
        )],
        &[],
    )
    .unwrap();
    drop(repo);

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "DROP TABLE change_captures;
         DROP TABLE change_sets;
         DELETE FROM schema_migrations WHERE version = '0014_change_captures';",
    )
    .unwrap();
    drop(conn);

    for _ in 0..2 {
        let repo = SqliteRepository::open(&db).unwrap();
        assert_eq!(
            repo.get_metadata(MetadataKey::RepoOwner)
                .unwrap()
                .as_deref(),
            Some("acme")
        );
        assert_eq!(
            repo.get_metadata(MetadataKey::CurrentBranch)
                .unwrap()
                .as_deref(),
            Some("topic")
        );
        assert_eq!(repo.get_branch_head("topic").unwrap().as_ref(), Some(&head));
        assert!(repo.get_commit(&head).unwrap().is_some());
        assert_eq!(repo.load_stat_cache().unwrap()["file.txt"].size, 7);
        assert!(matches!(
            repo.get_change_capture(CAPTURE_A).unwrap_err(),
            OakError::ChangeCaptureNotFound { .. }
        ));
    }

    let read_only = SqliteRepository::open_read_only(&db).unwrap();
    assert_eq!(
        read_only
            .get_metadata(MetadataKey::RepoName)
            .unwrap()
            .as_deref(),
        Some("widgets")
    );
}
