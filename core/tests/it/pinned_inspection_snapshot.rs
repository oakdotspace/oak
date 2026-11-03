//! fb-460 / fb-519: refs and tree inspections read ONE pinned snapshot. A
//! writer that commits after `open_read_only` must be invisible to them.
use oak_core::sqlite::tree_inspect::TreeInspectionLimits;
use oak_core::{Branch, FileMode, ManifestEntry, Repository, SqliteRepository};

fn commit(
    repo: &SqliteRepository,
    content: &[u8],
    parent: Option<oak_core::Hash>,
) -> oak_core::Hash {
    let blob = repo.put_blob(content.to_vec()).unwrap();
    let tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "f".into(),
            blob_hash: blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    repo.put_commit(
        "main".into(),
        parent,
        None,
        tree,
        "tester".into(),
        None,
        chrono::Utc::now(),
        vec![],
    )
    .unwrap()
}

#[test]
fn refs_and_tree_ignore_writes_committed_after_the_snapshot_opened() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("oak.db");
    let writer = SqliteRepository::open(&db).unwrap();
    let old = commit(&writer, b"old", None);
    writer
        .store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    writer.set_branch_head("topic", &old).unwrap();
    writer.set_current_branch("topic").unwrap();
    writer.set_head(&old).unwrap();

    let reader = SqliteRepository::open_read_only(&db).unwrap();

    // Committed after the reader pinned its snapshot.
    let new = commit(&writer, b"new bytes", Some(old.clone()));
    writer.set_branch_head("topic", &new).unwrap();
    writer.set_head(&new).unwrap();

    let refs = reader.inspect_refs(100).unwrap();
    assert_eq!(refs.effective_head.commit.as_deref(), Some(old.as_str()));
    assert_eq!(refs.legacy_head.as_deref(), Some(old.as_str()));
    assert!(refs.disagreements.is_empty(), "{:?}", refs.disagreements);

    let tree = reader
        .inspect_pinned_tree("HEAD", TreeInspectionLimits::default())
        .unwrap();
    assert_eq!(tree.commit.as_deref(), Some(old.as_str()));
    assert!(tree.complete);
    assert_eq!(tree.files[0].size, Some(3));

    // A fresh snapshot sees the new state.
    let fresh = SqliteRepository::open_read_only(&db).unwrap();
    let refs = fresh.inspect_refs(100).unwrap();
    assert_eq!(refs.effective_head.commit.as_deref(), Some(new.as_str()));
}

#[test]
fn inspections_refuse_writable_connections() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
    assert!(repo.inspect_refs(10).is_err());
    assert!(repo
        .inspect_pinned_tree("HEAD", TreeInspectionLimits::default())
        .is_err());
}
