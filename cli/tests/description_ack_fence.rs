use oak_core::{Branch, Repository, SqliteRepository};

#[test]
fn acknowledged_post_fences_get_started_after_edit_but_before_publication() {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
    let remote = Branch::new("feature".into(), Some("old remote".into()), None);
    repo.store_branch(&remote).unwrap();
    repo.edit_branch_description("feature", "published local")
        .unwrap();
    let post_generation = repo.description_publication_generation().unwrap();
    // GET begins while publication is in flight; its response still contains old remote text.
    let fetch_generation = repo.description_generation().unwrap();
    repo.acknowledge_branch_description("feature", Some("published local"), post_generation)
        .unwrap();
    assert!(!repo.branch_description_pending("feature").unwrap());
    repo.reconcile_pulled_branch_metadata(&remote, fetch_generation)
        .unwrap();
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("published local")
    );
    // The fence rejects only the old request, not future remote updates.
    let fresh_generation = repo.description_generation().unwrap();
    let newer = Branch::new("feature".into(), Some("later remote".into()), None);
    repo.reconcile_pulled_branch_metadata(&newer, fresh_generation)
        .unwrap();
    assert_eq!(
        repo.get_branch("feature")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("later remote")
    );
}
