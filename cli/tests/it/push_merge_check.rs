//! `oak push` reports conflicts with the branch's parent in the command that
//! published them. The check is advisory: publication has already landed, so
//! no answer from the review API may change the push's exit code.
use oak_core::{MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TARGET_HEAD: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct Pushed {
    output: std::process::Output,
    head: String,
}

impl Pushed {
    fn receipt(&self) -> Value {
        serde_json::from_slice(&self.output.stdout).unwrap()
    }

    fn stderr(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.output.stderr),
            String::from_utf8_lossy(&self.output.stdout)
        )
    }
}

/// Publish a one-commit branch to a mock server that answers `merge-preview`
/// with `preview`, or with a bare 404 like an older server when it is `None`.
async fn push(args: &[&str], preview: Option<ResponseTemplate>) -> Pushed {
    run(args, preview, false).await
}

/// [`push`] for commands that commit first: the head they publish does not
/// exist yet, so the mock cannot pin it.
async fn push_dirty(args: &[&str], preview: ResponseTemplate) -> Pushed {
    run(args, Some(preview), true).await
}

async fn run(args: &[&str], preview: Option<ResponseTemplate>, dirty: bool) -> Pushed {
    let temp = TempDir::new().unwrap();
    oak_cli::commands::init::run(temp.path(), false).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "work\n").unwrap();
    oak_cli::commands::commit::run(temp.path()).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap().to_string();
    if dirty {
        std::fs::write(temp.path().join("more.txt"), "more work\n").unwrap();
    }

    let server = MockServer::start().await;
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "agents").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();
    Mock::given(method("GET"))
        .and(path("/api/agents/widget"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "head": null })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/agents/widget/branches/{branch}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/agents/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": if dirty { None } else { Some(head.clone()) },
            "message": "Pushed"
        })))
        .mount(&server)
        .await;
    if let Some(preview) = preview {
        let mock = Mock::given(method("GET"))
            .and(path(format!(
                "/api/agents/widget/branches/{branch}/merge-preview"
            )))
            .and(query_param("against", "main"));
        let mock = if dirty {
            mock.respond_with(preview)
        } else {
            mock.and(query_param("branch_head", head.as_str()))
                .respond_with(preview)
        };
        mock.mount(&server).await;
    }

    let root = temp.path().to_path_buf();
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(root)
            .env("OAK_NO_UPDATE_CHECK", "1")
            // `oak finish` refuses to publish without a credential; the mock
            // ignores it.
            .env("OAK_API_KEY", "test-key")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    Pushed { output, head }
}

fn preview_body(clean: Value, files: Value, reason: Value) -> Value {
    json!({
        "schema_version": 1,
        "pins": { "branch_head": "ignored", "target_head": TARGET_HEAD, "diff_base": null },
        "prediction": "available",
        "reason": reason,
        "clean": clean,
        "conflict_file_count": files.as_array().unwrap().len(),
        "conflict_files": files,
        "conflict_files_truncated": false,
        "conflicts_restricted": 0,
        "invariant_violations": [],
    })
}

fn conflicting() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(preview_body(
        json!(false),
        json!([{ "path": "src/lib.rs", "kind": "text" }]),
        Value::Null,
    ))
}

#[tokio::test]
async fn json_receipt_names_conflicts_and_leads_with_the_fix() {
    let pushed = push(&["push", "--json"], Some(conflicting())).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let receipt = pushed.receipt();
    assert_eq!(receipt["published"], true);
    let check = &receipt["merge_check"];
    assert_eq!(check["status"], "conflicts");
    assert_eq!(check["target"], "main");
    assert_eq!(check["branch_head"], pushed.head.as_str());
    assert_eq!(check["target_head"], TARGET_HEAD);
    assert_eq!(check["conflict_file_count"], 1);
    assert_eq!(check["conflict_files"][0]["path"], "src/lib.rs");
    assert_eq!(check["conflict_files"][0]["kind"], "text");
    assert_eq!(receipt["recommended_next_commands"][0], "oak pull --json");
}

#[tokio::test]
async fn human_push_warns_with_paths_and_still_succeeds() {
    let pushed = push(&["push"], Some(conflicting())).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let stderr = pushed.stderr();
    assert!(stderr.contains("conflicts with main in 1 file"), "{stderr}");
    assert!(stderr.contains("src/lib.rs (text)"), "{stderr}");
    assert!(stderr.contains("oak pull --continue"), "{stderr}");
}

#[tokio::test]
async fn clean_branch_is_reported_as_clean_and_stays_quiet() {
    let clean =
        ResponseTemplate::new(200).set_body_json(preview_body(json!(true), json!([]), Value::Null));
    let pushed = push(&["push", "--json"], Some(clean.clone())).await;
    let receipt = pushed.receipt();
    assert_eq!(receipt["merge_check"]["status"], "clean");
    // Branch pushes don't run CI, so CI is never the next step here.
    let next = receipt["recommended_next_commands"].to_string();
    assert!(!next.contains("oak ci"), "{next}");
    assert!(next.contains("--merge-preview"), "{next}");

    let pushed = push(&["push"], Some(clean)).await;
    assert!(!pushed.stderr().contains("conflict"), "{}", pushed.stderr());
}

#[tokio::test]
async fn older_server_is_unknown_and_silent() {
    let pushed = push(&["push", "--json"], None).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let check = pushed.receipt()["merge_check"].clone();
    assert_eq!(check["status"], "unknown");
    assert_eq!(check["reason"], "server_unsupported");

    let pushed = push(&["push"], None).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    assert!(!pushed.stderr().contains("conflict"), "{}", pushed.stderr());
}

#[tokio::test]
async fn server_failure_never_fails_a_published_push() {
    let failing = ResponseTemplate::new(500).set_body_string("boom");
    let pushed = push(&["push", "--json"], Some(failing)).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let receipt = pushed.receipt();
    assert_eq!(receipt["published"], true);
    assert_eq!(receipt["merge_check"]["status"], "unknown");
    assert_eq!(receipt["merge_check"]["reason"], "request_failed");
}

#[tokio::test]
async fn undecided_prediction_is_not_a_conflict() {
    let undecided = ResponseTemplate::new(200).set_body_json(preview_body(
        Value::Null,
        json!([{ "path": "src/lib.rs", "kind": "blob_unavailable" }]),
        json!("blob_unavailable"),
    ));
    let pushed = push(&["push", "--json"], Some(undecided)).await;
    let check = pushed.receipt()["merge_check"].clone();
    assert_eq!(check["status"], "unknown");
    assert_eq!(check["reason"], "blob_unavailable");
    assert_eq!(check["conflict_file_count"], 0);
}

#[tokio::test]
async fn commit_push_carries_the_verdict() {
    let pushed = push_dirty(&["commit", "--push", "--json"], conflicting()).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let receipt = pushed.receipt();
    assert_eq!(receipt["pushed"], true);
    assert_eq!(receipt["merge_check"]["status"], "conflicts");
    assert_eq!(receipt["next_commands"][0], "oak pull --json");
}

#[tokio::test]
async fn finish_carries_the_verdict() {
    let pushed = push_dirty(&["finish", "--desc", "work", "--json"], conflicting()).await;
    assert!(pushed.output.status.success(), "{}", pushed.stderr());
    let receipt = pushed.receipt();
    assert_eq!(receipt["phase"], "complete");
    assert_eq!(receipt["merge_check"]["status"], "conflicts");
}
