//! `oak desc` outside a mount must sync the description to the server.
//!
//! The squash-merge message is the branch description, and the documented
//! agent workflow sets it *after* the last push (`commit; push; desc`). A
//! commit-less `oak push` returns "Already up to date" before sending branch
//! metadata, so without an explicit sync in `oak desc` the description never
//! leaves the machine — merges land with an empty message. These tests pin
//! the sync (and its failure modes) against a wiremock server.

use std::path::Path;

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal on-disk repo that `resolve::resolve` can find (`.oak/oak.db`),
/// on a current branch, optionally linked to a remote.
fn fixture_repo(dir: &Path, remote: Option<&str>) -> SqliteRepository {
    let oak_dir = dir.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.store_branch(&Branch::new(
        "tester-b1".to_string(),
        Some("initial".to_string()),
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("tester-b1").unwrap();
    if let Some(url) = remote {
        repo.set_metadata(MetadataKey::RemoteUrl, url).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    }
    repo
}

#[tokio::test(flavor = "current_thread")]
async fn desc_syncs_to_server_as_metadata_only_push() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = fixture_repo(temp.path(), None);

    let server = MockServer::start().await;
    // The sync must be the metadata-only shape: the new description in the
    // branch row, and no commits (desc must not push work as a side effect).
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .and(body_partial_json(serde_json::json!({
            "branch": { "name": "tester-b1", "description": "the real summary" },
            "commits": [],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();

    oak_cli::output::begin_capture();
    oak_cli::commands::branch::edit_current_branch(temp.path(), "the real summary")
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(
        captured.contains("Description synced to server"),
        "got: {captured}"
    );
    let br = repo.get_branch("tester-b1").unwrap().unwrap();
    assert_eq!(br.description.as_deref(), Some("the real summary"));
}

#[tokio::test(flavor = "current_thread")]
async fn desc_without_remote_stays_local_and_silent() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = fixture_repo(temp.path(), None);

    oak_cli::output::begin_capture();
    oak_cli::commands::branch::edit_current_branch(temp.path(), "local only")
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(
        !captured.contains("sync"),
        "no remote configured — desc must not mention syncing: {captured}"
    );
    let br = repo.get_branch("tester-b1").unwrap().unwrap();
    assert_eq!(br.description.as_deref(), Some("local only"));
}

/// The push endpoint signals rejection as HTTP 200 + `success: false`; the
/// desc sync must surface that as the "saved locally" warning, not claim
/// success — and the local update must still stick.
#[tokio::test(flavor = "current_thread")]
async fn desc_sync_rejection_warns_and_keeps_local_update() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = fixture_repo(temp.path(), None);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": false,
            "new_head": null,
            "message": "Conflict: remote head has changed. Pull first."
        })))
        .expect(1)
        .mount(&server)
        .await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();

    oak_cli::output::begin_capture();
    oak_cli::commands::branch::edit_current_branch(temp.path(), "rejected upstream")
        .await
        .unwrap();
    let captured = oak_cli::output::end_capture();

    assert!(
        captured.contains("couldn't sync to server"),
        "got: {captured}"
    );
    assert!(
        !captured.contains("Description synced to server"),
        "success:false must not read as synced: {captured}"
    );
    let br = repo.get_branch("tester-b1").unwrap().unwrap();
    assert_eq!(br.description.as_deref(), Some("rejected upstream"));
}
