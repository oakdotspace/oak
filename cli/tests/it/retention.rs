//! `oak retention`: lists via `GET /api/{owner}/{name}/retention`, sets via
//! `PUT`, removes via `DELETE ?path=`.

use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn lists_rules() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/widgets/retention"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "rules": [{"path": "build", "keep_versions": 3, "expired_blobs": 4,
                       "expired_bytes": 4096, "updated_at": "2026-11-08T00:00:00Z"}],
            "status": {"run_requested": false, "expired_blobs": 4, "expired_bytes": 4096},
            "can_edit": true
        })))
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let out = json(
        &oak(
            home.path(),
            &[
                "retention",
                "--repo",
                "acme/widgets",
                "-r",
                &server.uri(),
                "--json",
            ],
        )
        .await,
    );
    assert_eq!(out["repo"], "acme/widgets");
    assert_eq!(out["rules"][0]["path"], "build");
    assert_eq!(out["rules"][0]["keep_versions"], 3);
    assert_eq!(out["can_edit"], true);

    let human = oak(
        home.path(),
        &[
            "retention",
            "list",
            "--repo",
            "acme/widgets",
            "-r",
            &server.uri(),
        ],
    )
    .await;
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("build/"), "{text}");
    assert!(text.contains("keep 3"), "{text}");
}

#[tokio::test]
async fn sets_and_removes_a_rule() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/acme/widgets/retention"))
        .and(body_json(
            serde_json::json!({ "path": "build/out", "keep_versions": 2 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "path": "build/out", "keep_versions": 2, "created": false,
            "previous_keep_versions": 5
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/acme/widgets/retention"))
        .and(query_param("path", "build/out"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "path": "build/out", "deleted": true })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let set = json(
        &oak(
            home.path(),
            &[
                "retention",
                "set",
                "/build/out/",
                "--keep",
                "2",
                "--repo",
                "acme/widgets",
                "-r",
                &server.uri(),
                "--json",
            ],
        )
        .await,
    );
    assert_eq!(set["action"], "set");
    assert_eq!(set["keep_versions"], 2);
    assert_eq!(set["previous_keep_versions"], 5);
    let removed = json(
        &oak(
            home.path(),
            &[
                "retention",
                "remove",
                "build/out",
                "--repo",
                "acme/widgets",
                "-r",
                &server.uri(),
                "--json",
            ],
        )
        .await,
    );
    assert_eq!(removed["action"], "remove");
    assert_eq!(removed["path"], "build/out");
}

#[tokio::test]
async fn refuses_keep_zero_and_reports_server_refusals() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/acme/widgets/retention"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": "only organization owners and admins can change retention rules",
            "code": "forbidden"
        })))
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let zero = oak(
        home.path(),
        &[
            "retention",
            "set",
            "build",
            "--keep",
            "0",
            "--repo",
            "acme/widgets",
            "-r",
            &server.uri(),
        ],
    )
    .await;
    assert!(!zero.status.success());
    let forbidden = oak(
        home.path(),
        &[
            "retention",
            "set",
            "build",
            "--keep",
            "2",
            "--repo",
            "acme/widgets",
            "-r",
            &server.uri(),
        ],
    )
    .await;
    assert!(!forbidden.status.success());
    assert!(String::from_utf8_lossy(&forbidden.stderr).contains("owners and admins"));
}
