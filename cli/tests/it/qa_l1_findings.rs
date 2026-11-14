//! QA-L1 acceptance tests, adopted from independent QA of mrmrs-0a1cc5.
//! Each was RED on d7d2da0a and must stay GREEN. The ones that need a real
//! server are tested against it.

use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIAG: &str = "PRIVATE_REMOTE_DIAGNOSTIC sk_live_abc123";

fn oak_env(dir: &Path, home: &Path, args: &[&str], api_key: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null());
    match api_key {
        Some(key) => command.env("OAK_API_KEY", key),
        None => command.env_remove("OAK_API_KEY"),
    };
    command.output().expect("oak binary should run")
}

/// F1: `oak repo list` must not echo raw server error bodies for any status,
/// not only 401/403 (5xx/404 bodies can carry server diagnostics).
#[tokio::test]
async fn qa_repo_list_never_echoes_server_error_bodies() {
    for status in [404u16, 500, 502] {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/repos"))
            .respond_with(ResponseTemplate::new(status).set_body_string(DIAG))
            .mount(&server)
            .await;
        let home = tempfile::TempDir::new().unwrap();
        let uri = server.uri();
        let home_path = home.path().to_path_buf();
        let (json_out, human_out) = tokio::task::spawn_blocking(move || {
            (
                oak_env(
                    &home_path,
                    &home_path,
                    &["repo", "list", "--json", "--remote", &uri],
                    None,
                ),
                oak_env(
                    &home_path,
                    &home_path,
                    &["repo", "list", "--remote", &uri],
                    None,
                ),
            )
        })
        .await
        .unwrap();
        for output in [json_out, human_out] {
            assert!(!output.status.success());
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !all.contains("PRIVATE_REMOTE_DIAGNOSTIC"),
                "HTTP {status} body echoed: {all}"
            );
        }
    }
}
