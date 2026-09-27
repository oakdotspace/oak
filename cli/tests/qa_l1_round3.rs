//! QA-L1 round 3 acceptance test, adopted from independent QA of
//! mrmrs-0a1cc5. RED at 64910ed4: `oak repo list --remote R --json`
//! recommended `oak clone --shallow -- OWNER/NAME` without R, so running it
//! literally cloned from the default remote, not from R. Must stay GREEN.

use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn qa_repo_list_clone_recommendation_targets_the_listed_remote() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "repos": [{"owner": "-org", "name": "--help"}]
        })))
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let uri = server.uri();
    let home_path = home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(["repo", "list", "--json", "--remote", &uri])
            .current_dir(&home_path)
            .env("HOME", &home_path)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_REMOTE")
            .env_remove("OAK_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let clone = value["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .find(|c| c.starts_with("oak clone"))
        .expect("a clone recommendation");
    let separator = clone.find(" -- ").expect("positional separator");
    let remote_flag = format!("--remote={}", server.uri());
    let flag_at = clone.find(&remote_flag);
    assert!(
        matches!(flag_at, Some(at) if at < separator),
        "clone recommendation must name the listed remote before `--`: {clone}"
    );
}
