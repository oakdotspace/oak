include!("change_capture.rs");

#[test]
fn release_gate_explicit_repository_root_rejects_present_unavailable_path() {
    let mut outcomes = Vec::new();
    for (marker, marker_name) in [
        (MetadataKey::RestrictedBlobs, "restricted"),
        (MetadataKey::KnownLostBlobs, "known-loss"),
    ] {
        for selector_kind in ["dot", "absolute-root", "root-plus-other"] {
            let (dir, _) = fixture();
            let base_hash = oak_core::hash_bytes(b"base bytes\n");
            let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
            repo.set_metadata(marker, base_hash.as_str()).unwrap();
            drop(repo);
            let conn = rusqlite::Connection::open(dir.path().join(".oak/oak.db")).unwrap();
            conn.execute("DELETE FROM blobs WHERE hash=?1", [base_hash.as_str()])
                .unwrap();
            drop(conn);
            let absolute_root = std::fs::canonicalize(dir.path())
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let selectors: Vec<&str> = match selector_kind {
                "dot" => vec!["."],
                "absolute-root" => vec![absolute_root.as_str()],
                "root-plus-other" => vec![".", "other.txt"],
                _ => unreachable!(),
            };

            let (output, receipt) = capture(dir.path(), &selectors);

            outcomes.push(format!(
                "{marker_name}/{selector_kind}: success={}, code={}",
                output.status.success(),
                receipt["error"]["code"].as_str().unwrap_or("none")
            ));
            let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
            assert!(
                !repo
                    .has_blob(&oak_core::hash_bytes(b"captured bytes\n"))
                    .unwrap(),
                "capture persisted excluded replacement bytes for {marker_name}/{selector_kind}"
            );
        }
    }
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.contains("success=false, code=capture_scope_excluded")),
        "explicit root-scope outcomes did not all reject present unavailable bytes: {outcomes:#?}"
    );
}

#[test]
fn release_gate_root_selector_does_not_skip_later_outside_repository_path() {
    let (dir, _) = fixture();
    let outside = dir.path().parent().unwrap().join("outside-selection");
    let outside = outside.to_str().unwrap();

    let (root_first_output, root_first_receipt) = capture(dir.path(), &[".", outside]);
    let (outside_first_output, outside_first_receipt) = capture(dir.path(), &[outside, "."]);

    assert!(
        !root_first_output.status.success(),
        "root-first selector skipped validation of later outside path: {root_first_receipt}"
    );
    assert_eq!(root_first_receipt["error"]["code"], "invalid_argument");
    assert!(
        !outside_first_output.status.success(),
        "outside-first selector unexpectedly succeeded: {outside_first_receipt}"
    );
    assert_eq!(outside_first_receipt["error"]["code"], "invalid_argument");
}

#[test]
fn release_gate_capture_does_not_mutate_refs_commits_metadata_or_stat_cache() {
    let (dir, head) = fixture();
    let db = dir.path().join(".oak/oak.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let counts_before: Vec<i64> = ["branches", "commits", "metadata", "stat_cache"]
        .into_iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .collect();
    let metadata_before: Vec<(String, String)> = conn
        .prepare("SELECT key,value FROM metadata ORDER BY key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(conn);

    let (output, receipt) = capture(dir.path(), &[]);
    assert!(output.status.success(), "{receipt}");

    let conn = rusqlite::Connection::open(&db).unwrap();
    let counts_after: Vec<i64> = ["branches", "commits", "metadata", "stat_cache"]
        .into_iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .collect();
    let metadata_after: Vec<(String, String)> = conn
        .prepare("SELECT key,value FROM metadata ORDER BY key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(counts_after, counts_before);
    assert_eq!(metadata_after, metadata_before);
    drop(conn);
    let repo = SqliteRepository::open(&db).unwrap();
    assert_eq!(repo.get_branch_head("topic").unwrap(), Some(head));
}
