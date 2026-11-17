use oak_core::{
    hash_bytes, CanonicalChangeSetV1, ChangeScopeV1, FileMode, Manifest, ManifestEntry,
};

fn entry(path: &str, bytes: &[u8], mode: FileMode) -> ManifestEntry {
    ManifestEntry {
        path: path.to_string(),
        blob_hash: hash_bytes(bytes),
        mode,
    }
}

#[test]
fn canonical_change_set_v1_has_a_stable_framed_identity() {
    let base = Manifest::new(vec![entry("a.txt", b"old\n", FileMode::Regular)]);
    let result = Manifest::new(vec![
        entry("a.txt", b"new\n", FileMode::Executable),
        entry("nested/b.txt", b"added\n", FileMode::Regular),
    ]);

    let change_set =
        CanonicalChangeSetV1::from_manifests(&base, &result, ChangeScopeV1::Full).unwrap();

    assert_eq!(change_set.schema_version(), 1);
    assert_eq!(change_set.object_format(), "oak_v1_blake3");
    assert_eq!(change_set.base_tree(), &base.hash);
    assert_eq!(change_set.result_tree(), &result.hash);
    assert_eq!(change_set.changes().len(), 2);
    assert_eq!(
        change_set.id().as_str(),
        "850fc9fb80eb443d9366e0691688aee469da4acc411ccdb04a98ad281d510fb5"
    );
}

#[test]
fn canonical_change_set_v1_rejects_a_claimed_tree_not_derived_from_entries() {
    let mut base = Manifest::new(vec![entry("a.txt", b"old\n", FileMode::Regular)]);
    base.hash = hash_bytes(b"an arbitrary claimed root");

    let error =
        CanonicalChangeSetV1::from_manifests(&base, &Manifest::empty(), ChangeScopeV1::Full)
            .unwrap_err();

    assert!(
        error.to_string().contains("base manifest claims tree"),
        "unexpected error: {error}"
    );
}

#[test]
fn canonical_change_set_v1_sorts_paths_and_frames_fields() {
    let base = Manifest::empty();
    let result = Manifest::new(vec![
        entry("bc", b"one", FileMode::Regular),
        entry("a", b"two", FileMode::Regular),
    ]);
    let left = CanonicalChangeSetV1::from_manifests(
        &base,
        &result,
        ChangeScopeV1::Paths {
            paths: vec!["bc".into(), "a".into(), "a".into()],
        },
    )
    .unwrap();
    let right_result = Manifest::new(vec![
        entry("c", b"one", FileMode::Regular),
        entry("ab", b"two", FileMode::Regular),
    ]);
    let right = CanonicalChangeSetV1::from_manifests(
        &base,
        &right_result,
        ChangeScopeV1::Paths {
            paths: vec!["ab".into(), "c".into()],
        },
    )
    .unwrap();

    assert_eq!(
        left.scope(),
        &ChangeScopeV1::Paths {
            paths: vec!["a".into(), "bc".into()]
        }
    );
    assert_eq!(
        left.changes()
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "bc"]
    );
    assert_ne!(
        left.id(),
        right.id(),
        "field boundaries must affect identity"
    );
}

#[test]
fn canonical_change_set_v1_rejects_non_v1_blob_identities() {
    for invalid in ["not-a-hash".to_string(), "a".repeat(40), "A".repeat(64)] {
        let result = Manifest::new(vec![ManifestEntry {
            path: "a".into(),
            blob_hash: oak_core::Hash(invalid),
            mode: FileMode::Regular,
        }]);

        assert!(CanonicalChangeSetV1::from_manifests(
            &Manifest::empty(),
            &result,
            ChangeScopeV1::Full,
        )
        .is_err());
    }
}

#[test]
fn canonical_change_set_v1_enforces_exact_or_descendant_path_scope() {
    let result = Manifest::new(vec![
        entry("dir/a", b"a", FileMode::Regular),
        entry("outside", b"b", FileMode::Regular),
    ]);

    assert!(CanonicalChangeSetV1::from_manifests(
        &Manifest::empty(),
        &result,
        ChangeScopeV1::Paths { paths: vec![] },
    )
    .is_err());
    assert!(CanonicalChangeSetV1::from_manifests(
        &Manifest::empty(),
        &result,
        ChangeScopeV1::Paths {
            paths: vec!["dir".into()],
        },
    )
    .is_err());

    let only_directory = Manifest::new(vec![entry("dir/a", b"a", FileMode::Regular)]);
    CanonicalChangeSetV1::from_manifests(
        &Manifest::empty(),
        &only_directory,
        ChangeScopeV1::Paths {
            paths: vec!["dir".into()],
        },
    )
    .unwrap();
}
