//! `oak site` against a stub Pages API: request shapes, `--json`
//! passthrough, server error messages, and exit codes.

use std::path::Path;

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A linked acme/web checkout at `<home>/web`; returns its path.
fn checkout(home: &Path, remote: &str) -> std::path::PathBuf {
    let root = home.join("web");
    let oak_dir = root.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "web").unwrap();
    root
}

fn site_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "url": format!("https://{name}.oak.page/"),
        "repo": "acme/web",
        "source_dir": "public",
        "spa_fallback": true,
        "password_protected": false,
        "disabled": false,
        "disabled_reason": null,
        "deployment": null,
        "created_at": "2026-11-07T00:00:00Z",
        "updated_at": "2026-11-07T00:00:00Z"
    })
}

async fn oak(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .args(args)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn create_posts_the_contract_body_and_prints_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/acme/web/pages/sites"))
        .and(body_json(serde_json::json!({
            "name": "docs", "source_dir": "public", "spa_fallback": true
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(site_json("docs")))
        .expect(2)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let web = checkout(home.path(), &server.uri());

    let out = oak(
        home.path(),
        &web,
        &["site", "create", "docs", "--dir", "public", "--spa"],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("https://docs.oak.page/"));

    let out = oak(
        home.path(),
        &web,
        &[
            "site", "create", "docs", "--dir", "public", "--spa", "--json",
        ],
    )
    .await;
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc, site_json("docs"));
}

#[tokio::test]
async fn failed_redeploy_prints_the_reason_and_exits_nonzero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/acme/web/pages/sites/docs/deployments"))
        .and(body_json(serde_json::json!({ "action": "redeploy" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 9, "status": "failed", "failure": "source_dir public not found",
            "commit": null, "file_count": null, "total_bytes": null,
            "origin": "manual", "created_by": "ann", "created_at": "2026-11-07T00:00:00Z"
        })))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let web = checkout(home.path(), &server.uri());

    let out = oak(home.path(), &web, &["site", "redeploy", "docs", "--json"]).await;
    assert_eq!(out.status.code(), Some(1));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["status"], "failed");
    assert!(String::from_utf8_lossy(&out.stderr).contains("source_dir public not found"));
}

#[tokio::test]
async fn server_errors_surface_the_message_with_the_network_exit_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/web/pages/sites/docs"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({ "error": "site not found" })),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let web = checkout(home.path(), &server.uri());

    let out = oak(home.path(), &web, &["site", "show", "docs"]).await;
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("site not found"));
}

#[tokio::test]
async fn rm_without_yes_refuses_off_a_terminal_and_never_calls_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let web = checkout(home.path(), &server.uri());

    let out = oak(home.path(), &web, &["site", "rm", "docs"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
}

#[tokio::test]
async fn check_reports_availability_through_the_exit_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/pages/names/www"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "www", "available": false, "reason": "reserved"
        })))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    // No checkout needed: pass the remote explicitly from an empty dir.
    let out = oak(
        home.path(),
        home.path(),
        &["site", "check", "www", "-r", &server.uri()],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reserved")
            || String::from_utf8_lossy(&out.stdout).contains("reserved")
    );
}
