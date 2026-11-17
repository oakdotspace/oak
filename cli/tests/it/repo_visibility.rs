//! `oak repo visibility`: reads via `GET /api/{owner}/{name}`, writes via
//! `PATCH /api/{owner}/{name}/visibility`, never echoes server error bodies.

use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIAG: &str = "PRIVATE_REMOTE_DIAGNOSTIC sk_live_abc123";

async fn oak(home: &Path, args: &[&str]) -> Output {
    let home = home.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(&args)
            .current_dir(&home)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_API_KEY", "test-key")
            .env_remove("OAK_REMOTE")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("oak binary should run")
    })
    .await
    .unwrap()
}

async fn repo_is(server: &MockServer, is_public: bool) {
    Mock::given(method("GET"))
        .and(path("/api/acme/widgets"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widgets", "owner": "acme", "is_public": is_public,
        })))
        .mount(server)
        .await;
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn shows_visibility_without_writing() {
    let server = MockServer::start().await;
    repo_is(&server, false).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();

    let out = json(
        &oak(
            home.path(),
            &[
                "repo",
                "visibility",
                "--repo",
                "acme/widgets",
                "--json",
                "-r",
                &server.uri(),
            ],
        )
        .await,
    );
    assert_eq!(out["visibility"], "private");
    assert_eq!(out["is_public"], false);
    assert!(out.get("changed").is_none());
    assert_eq!(
        out["recommended_next_commands"][0],
        "oak repo visibility public --repo acme/widgets"
    );
}

#[tokio::test]
async fn makes_a_repo_public() {
    let server = MockServer::start().await;
    repo_is(&server, false).await;
    Mock::given(method("PATCH"))
        .and(path("/api/acme/widgets/visibility"))
        .and(body_json(serde_json::json!({ "is_public": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "widgets", "is_public": true,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();

    let out = json(
        &oak(
            home.path(),
            &[
                "repo",
                "visibility",
                "public",
                "--repo",
                "acme/widgets",
                "--json",
                "-r",
                &server.uri(),
            ],
        )
        .await,
    );
    assert_eq!(out["visibility"], "public");
    assert_eq!(out["changed"], true);
}

#[tokio::test]
async fn setting_the_current_visibility_is_a_no_op() {
    let server = MockServer::start().await;
    repo_is(&server, true).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();

    let out = json(
        &oak(
            home.path(),
            &[
                "repo",
                "visibility",
                "public",
                "--repo",
                "acme/widgets",
                "--json",
                "-r",
                &server.uri(),
            ],
        )
        .await,
    );
    assert_eq!(out["changed"], false);
}

#[tokio::test]
async fn non_admin_gets_a_clear_refusal() {
    let server = MockServer::start().await;
    repo_is(&server, true).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(403).set_body_string(DIAG))
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();

    let out = oak(
        home.path(),
        &[
            "repo",
            "visibility",
            "private",
            "--repo",
            "acme/widgets",
            "-r",
            &server.uri(),
        ],
    )
    .await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("owners and admins"), "{stderr}");
    assert!(!stderr.contains("PRIVATE_REMOTE_DIAGNOSTIC"), "{stderr}");
}

#[tokio::test]
async fn never_echoes_server_error_bodies() {
    for status in [404u16, 500, 502] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(DIAG))
            .mount(&server)
            .await;
        let home = tempfile::TempDir::new().unwrap();
        for args in [
            vec!["repo", "visibility", "--repo", "acme/widgets", "--json"],
            vec!["repo", "visibility", "public", "--repo", "acme/widgets"],
        ] {
            let mut args = args;
            let uri = server.uri();
            args.extend(["-r", &uri]);
            let out = oak(home.path(), &args).await;
            assert!(!out.status.success());
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !all.contains("PRIVATE_REMOTE_DIAGNOSTIC"),
                "HTTP {status} body echoed: {all}"
            );
        }
    }
}
