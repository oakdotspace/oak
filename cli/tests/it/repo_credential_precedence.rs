//! Repository-scoped commands must all bind the same normalized credential.
//! These process-isolated tests pin the precedence contract without mutating
//! the test runner's HOME or OAK_API_KEY.

use std::path::Path;

use oak_core::{MetadataKey, Repository, SqliteRepository};
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
        .env("OAK_NO_UPDATE_CHECK", "1");
    command
}

#[tokio::test]
async fn site_request_prefers_repo_token_when_environment_is_blank() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/pages/sites"))
        .and(header("authorization", format!("Bearer {REPO_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "sites": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let home = tempfile::tempdir().unwrap();
    let checkout = home.path().join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let _repo = fixture_repo(&checkout, &server.uri());
    write_account_credential(home.path(), &server.uri());

    let output = oak_command(home.path(), &checkout)
        .args(["site", "list", "--remote", &server.uri()])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
