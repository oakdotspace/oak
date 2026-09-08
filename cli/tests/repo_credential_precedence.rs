//! Repository-scoped commands must all bind the same normalized credential.
//! These process-isolated tests pin the precedence contract without mutating
//! the test runner's HOME or OAK_API_KEY.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oak_cli::commands::mount;
use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO_TOKEN: &str = "repo-token-a";
const ACCOUNT_TOKEN: &str = "stored-token-b";

fn write_account_credential(home: &Path, remote: &str) {
    let oak_dir = home.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    std::fs::write(
        oak_dir.join("credentials"),
        serde_json::to_vec_pretty(&serde_json::json!([{
            "server": remote,
            "token": ACCOUNT_TOKEN,
            "username": "account-user"
        }]))
        .unwrap(),
    )
    .unwrap();
}

fn fixture_repo(root: &Path, remote: &str) -> SqliteRepository {
    let oak_dir = root.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    repo.set_metadata(MetadataKey::ApiKey, REPO_TOKEN).unwrap();
    repo
}

fn oak_command(home: &Path, cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", " \t ")
        .env("OAK_FEATURES", "all")
        .env("OAK_NO_UPDATE_CHECK", "1");
    command
}

#[tokio::test]
async fn release_request_prefers_repo_token_when_environment_is_blank() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/releases"))
        .and(header("authorization", format!("Bearer {REPO_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "releases": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let checkout = home.path().join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let _repo = fixture_repo(&checkout, &server.uri());
    write_account_credential(home.path(), &server.uri());

    let output = oak_command(home.path(), &checkout)
        .args(["release", "list"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn site_request_prefers_repo_token_when_environment_is_blank() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/orgs/oak/site"))
        .and(header("authorization", format!("Bearer {REPO_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "organization_slug": "oak",
            "repo_name": "repo",
            "branch": "main",
            "source_dir": "/",
            "url": "https://oak.example/",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let checkout = home.path().join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let _repo = fixture_repo(&checkout, &server.uri());
    write_account_credential(home.path(), &server.uri());

    let output = oak_command(home.path(), &checkout)
        .args(["site", "enable", "--remote", &server.uri()])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture_mount(home: &Path, remote: &str) -> (PathBuf, PathBuf) {
    let destination = home.join("mount");
    std::fs::create_dir_all(&destination).unwrap();
    let mounts_root = home.join(".oak/mounts");
    let id = "credential-test";
    let state_dir = mounts_root.join(id);
    std::fs::create_dir_all(&state_dir).unwrap();

    let virtual_branch = "credential-test--12345678";
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    cache.set_metadata(MetadataKey::ApiKey, REPO_TOKEN).unwrap();
    cache
        .store_branch(&Branch::new(
            virtual_branch.to_string(),
            Some("before".to_string()),
            Some("main".to_string()),
        ))
        .unwrap();

    mount::state::save_config(
        &state_dir,
        &mount::state::MountConfig {
            id: id.to_string(),
            mount_point: destination.clone(),
            remote_url: remote.to_string(),
            owner: "oak".to_string(),
            repo: "repo".to_string(),
            base_branch: "main".to_string(),
            base_commit: "0".repeat(64),
            virtual_branch: virtual_branch.to_string(),
            mounted_branch: None,
        },
    )
    .unwrap();
    let index = mount::state::MountIndex {
        mounts: HashMap::from([(mount::state::canonical_key(&destination), id.to_string())]),
    };
    std::fs::write(
        mounts_root.join("index.json"),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
    (destination, mounts_root)
}

#[tokio::test]
async fn mount_request_prefers_cache_token_when_environment_is_blank() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oak/repo/push"))
        .and(header("authorization", format!("Bearer {REPO_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    write_account_credential(home.path(), &server.uri());
    let (destination, mounts_root) = fixture_mount(home.path(), &server.uri());
    let description = home.path().join("description.txt");
    std::fs::write(&description, "repository credential wins\n").unwrap();

    let output = oak_command(home.path(), &destination)
        .env("OAK_MOUNTS_ROOT", mounts_root)
        .args(["desc", "--file", description.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
