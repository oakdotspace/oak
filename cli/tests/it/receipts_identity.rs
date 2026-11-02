//! Receipt and identity contract for agent-facing JSON (fb-434/449 desc half,
//! fb-525, fb-168a, fb-371, fb-296, fb-165, fb-480).
//!
//! Every test runs the real `oak` binary with piped stdio and an isolated
//! HOME so no developer credential or remote can leak in.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "synthetic-secret-token-do-not-print";

fn oak_env(dir: &Path, home: &Path, args: &[&str], api_key: Option<&str>) -> Output {
    oak_env_with(dir, home, args, api_key, &[])
}

fn oak_env_with(
    dir: &Path,
    home: &Path,
    args: &[&str],
    api_key: Option<&str>,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command.envs(extra_env.iter().copied());
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_REMOTE")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null());
    match api_key {
        Some(key) => command.env("OAK_API_KEY", key),
        None => command.env_remove("OAK_API_KEY"),
    };
    command.output().expect("oak binary should run")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

struct Fixture {
    repo_dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let repo_dir = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        oak_cli::commands::init::run(repo_dir.path(), false).unwrap();
        Self { repo_dir, home }
    }

    fn repo(&self) -> SqliteRepository {
        SqliteRepository::open(&self.repo_dir.path().join(".oak/oak.db")).unwrap()
    }

    fn link(&self, remote: &str, owner: &str, name: &str) {
        let repo = self.repo();
        repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, owner).unwrap();
        repo.set_metadata(MetadataKey::RepoName, name).unwrap();
    }

    fn branch(&self) -> String {
        self.repo().get_current_branch_name().unwrap().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        oak_env(self.repo_dir.path(), self.home.path(), args, None)
    }

    fn run_with_key(&self, args: &[&str]) -> Output {
        oak_env(self.repo_dir.path(), self.home.path(), args, Some(SECRET))
    }

    /// The process cwd is canonical (macOS `/var` -> `/private/var`).
    fn root(&self) -> String {
        self.repo_dir
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    }
}

const IDENTITY_FIELDS: [&str; 6] = [
    "repo_owner",
    "repo_name",
    "remote_url",
    "repository_root",
    "web_url",
    "review_url",
];

// ---------------------------------------------------------------------------
// status / info / branch show identity (fb-168a, fb-371, fb-296)
// ---------------------------------------------------------------------------

#[test]
fn unlinked_checkout_reports_root_and_null_remote_identity() {
    let fixture = Fixture::new();
    let branch = fixture.branch();
    for args in [
        vec!["status", "--json"],
        vec!["info", "--json"],
        vec!["branch", "show", branch.as_str(), "--json"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let value = json(&output);
        for field in IDENTITY_FIELDS {
            assert!(
                value.as_object().unwrap().contains_key(field),
                "{args:?} lacks {field}: {value}"
            );
        }
        assert_eq!(value["repository_root"], fixture.root(), "{args:?}");
        // `oak init` records a local repo name; with no remote there is no
        // owner, remote, or web URL.
        for field in ["repo_owner", "remote_url", "web_url", "review_url"] {
            assert!(value[field].is_null(), "{args:?} {field}: {value}");
        }
        assert!(
            !value
                .as_object()
                .unwrap()
                .contains_key("workspace_classification"),
            "root is informational only"
        );
    }
}

#[test]
fn linked_checkout_reports_identity_and_web_urls() {
    let fixture = Fixture::new();
    fixture.link("https://oak.example/", "acme", "web");
    let branch = fixture.branch();
    let review = format!("https://oak.example/acme/web/branches/{branch}");
    for args in [vec!["status", "--json"], vec!["info", "--json"]] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let value = json(&output);
        assert_eq!(value["repo_owner"], "acme");
        assert_eq!(value["repo_name"], "web");
        assert_eq!(value["remote_url"], "https://oak.example/");
        assert_eq!(value["repository_root"], fixture.root());
        assert_eq!(value["web_url"], "https://oak.example/acme/web");
        assert_eq!(value["review_url"], review.as_str());
        // Existing fields are untouched (append-only schema).
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["branch"], branch.as_str());
    }

    // A subdirectory still reports the working-tree root.
    let sub = fixture.repo_dir.path().join("nested/dir");
    std::fs::create_dir_all(&sub).unwrap();
    let output = oak_env(&sub, fixture.home.path(), &["status", "--json"], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json(&output)["repository_root"], fixture.root());

    // branch show targets the shown branch, not the current one.
    fixture
        .repo()
        .store_branch(&oak_core::Branch::new(
            "other/topic".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
    let output = fixture.run(&["branch", "show", "other/topic", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["name"], "other/topic");
    assert_eq!(value["current"], false);
    assert_eq!(
        value["review_url"],
        "https://oak.example/acme/web/branches/other%2Ftopic"
    );
    assert_eq!(value["web_url"], "https://oak.example/acme/web");
}

// ---------------------------------------------------------------------------
// oak open --print / --json (fb-296)
// ---------------------------------------------------------------------------

#[test]
fn open_print_and_json_emit_url_without_launching() {
    let fixture = Fixture::new();
    fixture.link("https://oak.example", "acme", "web");
    let branch = fixture.branch();

    // Unpublished branch with no commits: the repo home (what `oak open` opens).
    let output = fixture.run(&["open", "--print"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://oak.example/acme/web\n"
    );

    std::fs::write(fixture.repo_dir.path().join("a.txt"), b"a\n").unwrap();
    oak_cli::commands::commit::run(fixture.repo_dir.path()).unwrap();
    let output = fixture.run(&["open", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    let review = format!("https://oak.example/acme/web/branches/{branch}");
    assert_eq!(value["url"], review.as_str());
    assert_eq!(value["target"], "branch");
    assert_eq!(value["launched"], false);
    assert_eq!(value["review_url"], review.as_str());
    assert_eq!(value["repository_root"], fixture.root());

    let output = fixture.run(&["open", "--print"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{review}\n")
    );
}

#[test]
fn open_json_on_unlinked_checkout_is_an_error_envelope() {
    let fixture = Fixture::new();
    let output = fixture.run(&["open", "--json"]);
    assert!(!output.status.success());
    let value = json(&output);
    assert_eq!(value["error"]["code"], "configuration_error");
}

// ---------------------------------------------------------------------------
// oak desc --json receipts (fb-434, fb-449 desc half, fb-525)
// ---------------------------------------------------------------------------

#[test]
fn desc_json_without_remote_is_local_only_not_synced() {
    let fixture = Fixture::new();
    let output = fixture.run(&["desc", "local words", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["branch"], fixture.branch().as_str());
    assert_eq!(value["description"], "local words");
    assert_eq!(value["local_saved"], true);
    assert_eq!(value["remote_synced"], "not_linked");
    assert_eq!(value["remote_state"], "not_linked");
    assert!(value.get("retry_command").is_none());
}

#[tokio::test]
async fn desc_json_file_reports_synced_against_loopback_serve() {
    let fixture = Fixture::new();
    let data = tempfile::TempDir::new().unwrap();
    std::fs::write(fixture.repo_dir.path().join("kept.txt"), b"work\n").unwrap();
    oak_cli::commands::commit::run(fixture.repo_dir.path()).unwrap();
    let server = oak_cli::commands::serve::spawn_loopback(data.path().join("data"))
        .await
        .unwrap();
    oak_cli::commands::push::run(
        fixture.repo_dir.path(),
        Some(&server),
        false,
        Some("qa/widget"),
    )
    .await
    .unwrap();
    let branch = fixture.branch();
    let desc_file = fixture.home.path().join("desc.txt");
    std::fs::write(&desc_file, "narrative from a file\n").unwrap();
    let desc_arg = desc_file.display().to_string();

    let dir = fixture.repo_dir.path().to_path_buf();
    let home = fixture.home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        oak_env(&dir, &home, &["desc", "--file", &desc_arg, "--json"], None)
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["branch"], branch.as_str());
    assert_eq!(value["description"], "narrative from a file\n");
    assert_eq!(value["local_saved"], true);
    assert_eq!(value["remote_synced"], true);
    assert_eq!(value["remote_state"], "synced");
    assert!(value.get("retry_command").is_none());

    let remote: serde_json::Value = reqwest::get(format!("{server}/api/qa/widget/pull"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(remote["branches"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |row| row["name"] == branch.as_str() && row["description"] == "narrative from a file\n"
        ));
}

async fn desc_against_status(status: u16) -> (Fixture, Output, String) {
    let fixture = Fixture::new();
    let server = MockServer::builder().start().await;
    fixture.link(&server.uri(), "qa", "widget");
    Mock::given(method("POST"))
        .and(path("/api/qa/widget/push"))
        .respond_with(ResponseTemplate::new(status).set_body_string("PRIVATE_REMOTE_DIAGNOSTIC"))
        .expect(1)
        .mount(&server)
        .await;
    let desc_file = fixture.home.path().join("my desc.txt");
    std::fs::write(&desc_file, "server may reject").unwrap();
    let desc_arg = desc_file.display().to_string();
    let dir = fixture.repo_dir.path().to_path_buf();
    let home = fixture.home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        oak_env(&dir, &home, &["desc", "--file", &desc_arg, "--json"], None)
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("PRIVATE_REMOTE_DIAGNOSTIC"), "{stdout}");
    (fixture, output, desc_file.display().to_string())
}

#[tokio::test]
async fn desc_json_rejected_sync_is_false_with_exact_retry() {
    let (fixture, output, desc_path) = desc_against_status(409).await;
    // The local save succeeded, so the command succeeds; the receipt carries
    // the remote failure.
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["local_saved"], true);
    assert_eq!(value["remote_synced"], false);
    assert_eq!(value["remote_state"], "failed");
    let retry = value["retry_command"].as_str().unwrap();
    assert_eq!(retry, format!("oak desc --json --file='{desc_path}'"));
    assert_eq!(value["recommended_next_commands"][0], retry);
    // A commit-less push does not send the description (QA F2).
    let everything = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!everything.contains("oak push"), "{everything}");
    assert_eq!(
        fixture
            .repo()
            .get_branch(&fixture.branch())
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("server may reject")
    );
}

#[tokio::test]
async fn desc_json_unconfirmed_sync_is_unknown_without_blind_retry() {
    let (fixture, output, _) = desc_against_status(503).await;
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["local_saved"], true);
    assert_eq!(value["remote_synced"], "unknown");
    assert_eq!(value["remote_state"], "unknown");
    assert!(value.get("retry_command").is_none(), "{value}");
    let next: Vec<&str> = value["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        next.contains(&format!("oak branch show {} --remote --json", fixture.branch()).as_str()),
        "{next:?}"
    );
    assert!(next.iter().all(|command| !command.starts_with("oak desc")));
}

#[tokio::test]
async fn desc_json_unreachable_server_is_never_reported_synced() {
    let fixture = Fixture::new();
    // Bind then drop: nothing listens on this port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let remote = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    fixture.link(&remote, "qa", "widget");
    let dir = fixture.repo_dir.path().to_path_buf();
    let home = fixture.home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        oak_env(&dir, &home, &["desc", "offline words", "--json"], None)
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["local_saved"], true);
    assert_ne!(value["remote_synced"], true, "{value}");
    assert_ne!(value["remote_state"], "synced");
    assert!(value["remote_error"].is_string());
}

#[test]
fn desc_json_missing_file_fails_nonzero_with_envelope() {
    let fixture = Fixture::new();
    let output = fixture.run(&["desc", "--file", "/definitely/not/here.txt", "--json"]);
    assert!(!output.status.success(), "{output:?}");
    let value = json(&output);
    assert!(value["error"]["code"].is_string(), "{value}");
    assert!(fixture
        .repo()
        .get_branch(&fixture.branch())
        .unwrap()
        .unwrap()
        .description
        .is_none_or(|d| d.is_empty()));
}

// ---------------------------------------------------------------------------
// oak repo list --json / oak space repos --json (fb-165)
// ---------------------------------------------------------------------------

async fn repo_server() -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/repos"))
        .and(header("authorization", format!("Bearer {SECRET}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "repos": [
                {"name": "alpha", "owner": "acme", "head": "aa", "is_public": true,
                 "description": "first", "updated_at": "2026-09-01T00:00:00Z"},
                {"name": "beta", "owner": "acme", "head": null, "is_public": false},
                {"name": "gamma", "owner": "other", "head": "cc"},
                {"name": "orphan", "owner": null}
            ]
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn repo_list_json_shape_filter_and_bound() {
    let server = repo_server().await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        let all = fixture.run_with_key(&["repo", "list", "--json", "--remote", &uri]);
        let org = fixture.run_with_key(&[
            "repo", "list", "--json", "--remote", &uri, "--org", "acme", "--limit", "1",
        ]);
        (all, org)
    })
    .await
    .unwrap();
    let (all, org) = output;
    assert!(all.status.success(), "{all:?}");
    let value = json(&all);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["authenticated"], true);
    assert_eq!(value["count"], 4);
    assert_eq!(value["truncated"], false);
    let alpha = &value["repos"][0];
    assert_eq!(alpha["owner"], "acme");
    assert_eq!(alpha["name"], "alpha");
    assert_eq!(alpha["full_name"], "acme/alpha");
    assert_eq!(alpha["default_branch_head"], "aa");
    assert_eq!(alpha["web_url"], format!("{}/acme/alpha", server.uri()));
    let orphan = &value["repos"][3];
    assert!(orphan["owner"].is_null() && orphan["full_name"].is_null());
    assert!(!String::from_utf8_lossy(&all.stdout).contains(SECRET));

    assert!(org.status.success(), "{org:?}");
    let value = json(&org);
    assert_eq!(value["org"], "acme");
    assert_eq!(value["count"], 1);
    assert_eq!(value["total_count"], 2);
    assert_eq!(value["truncated"], true);
    assert_eq!(value["repos"][0]["name"], "alpha");
    assert!(value["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .contains("--limit 2"));
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tokio::test]
async fn space_repos_json_shares_the_repo_list_payload() {
    let server = repo_server().await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run_with_key(&["space", "repos", "acme", "--json", "--remote", &uri])
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["org"], "acme");
    assert_eq!(value["count"], 2);
    assert_eq!(value["repos"][1]["full_name"], "acme/beta");
}

#[tokio::test]
async fn repo_list_auth_failures_are_typed() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/repos"))
        .respond_with(ResponseTemplate::new(401).set_body_string("PRIVATE_REMOTE_DIAGNOSTIC"))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run_with_key(&["repo", "list", "--json", "--remote", &uri])
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(6), "{output:?}");
    let value = json(&output);
    assert_eq!(value["error"]["code"], "auth_required");
    assert!(value["error"]["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .starts_with("oak login --remote="));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains(SECRET) && !all.contains("PRIVATE_REMOTE_DIAGNOSTIC"));
}

// ---------------------------------------------------------------------------
// oak auth status --json (fb-480)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_status_reports_source_identity_and_quiet_admin_404() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/whoami"))
        .and(header("authorization", format!("Bearer {SECRET}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "username": "ada", "email": "ada@example.com", "display_name": "Ada"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/feedback/capabilities"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run_with_key(&["auth", "status", "--json", "--remote", &uri])
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["remote"], server.uri());
    assert_eq!(value["remote_source"], "argument");
    assert_eq!(value["credential"]["source"], "env");
    assert_eq!(value["credential"]["present"], true);
    assert_eq!(value["identity"]["state"], "authenticated");
    assert_eq!(value["identity"]["username"], "ada");
    assert_eq!(value["capabilities"]["feedback_admin"], "absent_or_denied");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains(SECRET), "secret leaked: {all}");
    assert!(!all.contains("ada@example.com"), "email is not echoed");
}

#[tokio::test]
async fn auth_status_without_credential_is_unauthenticated_not_an_error() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/whoami"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/feedback/capabilities"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run(&["auth", "status", "--json", "--remote", &uri])
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["credential"]["source"], "none");
    assert_eq!(value["credential"]["present"], false);
    assert_eq!(value["identity"]["state"], "unauthenticated");
    assert!(value["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .starts_with("oak login --remote="));
}

// ---------------------------------------------------------------------------
// QA-L1 follow-ups: auth-rejection guidance (F4), no placeholders (F5),
// bounded read-only probes (F3), mount receipts and identity (F7)
// ---------------------------------------------------------------------------

async fn desc_inline_against_status(
    status: u16,
    args: &'static [&'static str],
) -> (Fixture, Output, String) {
    let fixture = Fixture::new();
    let server = MockServer::builder().start().await;
    fixture.link(&server.uri(), "qa", "widget");
    Mock::given(method("POST"))
        .and(path("/api/qa/widget/push"))
        .respond_with(ResponseTemplate::new(status).set_body_string("PRIVATE_REMOTE_DIAGNOSTIC"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = fixture.repo_dir.path().to_path_buf();
    let home = fixture.home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || oak_env(&dir, &home, args, None))
        .await
        .unwrap();
    (fixture, output, server.uri())
}

#[tokio::test]
async fn desc_json_auth_rejection_recommends_login_before_retry() {
    for status in [401u16, 403] {
        let (_fixture, output, uri) =
            desc_inline_against_status(status, &["desc", "needs login", "--json"]).await;
        assert!(output.status.success(), "{output:?}");
        let value = json(&output);
        assert_eq!(value["remote_synced"], false);
        let next = value["recommended_next_commands"].as_array().unwrap();
        assert_eq!(next[0], format!("oak login --remote={uri}"), "{value}");
        assert_eq!(next[1], "oak desc --json -- 'needs login'", "{value}");
        let guidance = value["retry_guidance"].as_str().unwrap();
        assert!(guidance.contains("oak login --remote"), "{guidance}");
        assert!(!guidance.contains("oak push"), "{guidance}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("oak login --remote"), "{stderr}");
        assert!(!stderr.contains("oak push"), "{stderr}");
    }
}

#[tokio::test]
async fn desc_json_never_emits_placeholder_commands() {
    // Inline text: the retry is the literal, shell-quoted command.
    let (_fixture, output, _) =
        desc_inline_against_status(409, &["desc", "it's inline", "--json"]).await;
    let value = json(&output);
    assert_eq!(
        value["retry_command"],
        r#"oak desc --json -- 'it'\''s inline'"#
    );

    // stdin can't be replayed: no retry command, and nothing placeholder-shaped.
    let (_fixture, output, _) =
        desc_inline_against_status(409, &["desc", "--file", "-", "--json"]).await;
    let value = json(&output);
    assert_eq!(value["remote_synced"], false);
    assert!(value.get("retry_command").is_none(), "{value}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("<FILE>"), "{text}");
    for command in value["recommended_next_commands"].as_array().unwrap() {
        assert!(!command.as_str().unwrap().contains('<'), "{command}");
    }
}

async fn stalled_server() -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn auth_status_and_repo_list_are_time_bounded() {
    let server = stalled_server().await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let started = std::time::Instant::now();
    let (auth, list) = tokio::task::spawn_blocking(move || {
        let env = [("OAK_PROBE_TIMEOUT_SECS", "1")];
        let dir = fixture.repo_dir.path();
        let home = fixture.home.path();
        (
            oak_env_with(
                dir,
                home,
                &["auth", "status", "--json", "--remote", &uri],
                Some(SECRET),
                &env,
            ),
            oak_env_with(
                dir,
                home,
                &["repo", "list", "--json", "--remote", &uri],
                Some(SECRET),
                &env,
            ),
        )
    })
    .await
    .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "probes must be bounded, took {:?}",
        started.elapsed()
    );

    assert!(auth.status.success(), "{auth:?}");
    let value = json(&auth);
    assert_eq!(value["identity"]["state"], "unreachable", "{value}");
    assert_eq!(value["identity"]["detail"], "timed out after 1s");

    assert_eq!(list.status.code(), Some(6), "{list:?}");
    let value = json(&list);
    assert_eq!(value["error"]["code"], "remote_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("timed out after 1s"),
        "{value}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn fake_mount(home: &Path, remote: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use oak_cli::commands::mount;
    let destination = home.join("mnt");
    std::fs::create_dir_all(&destination).unwrap();
    let mounts_root = home.join(".oak/mounts");
    let id = "receipt-test";
    let state_dir = mounts_root.join(id);
    std::fs::create_dir_all(&state_dir).unwrap();
    let virtual_branch = "receipt-test--12345678";
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    cache
        .store_branch(&oak_core::Branch::new(
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
            owner: "qa".to_string(),
            repo: "widget".to_string(),
            base_branch: "main".to_string(),
            base_commit: "0".repeat(64),
            virtual_branch: virtual_branch.to_string(),
            mounted_branch: None,
        },
    )
    .unwrap();
    let index = mount::state::MountIndex {
        mounts: std::collections::HashMap::from([(
            mount::state::canonical_key(&destination),
            id.to_string(),
        )]),
    };
    std::fs::write(
        mounts_root.join("index.json"),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
    (destination, mounts_root)
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tokio::test]
async fn mount_desc_json_receipt_and_identity_fields() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/qa/widget/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "new_head": null, "message": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let (destination, mounts_root) = fake_mount(home.path(), &server.uri());
    let uri = server.uri();
    let (desc, status, info) = tokio::task::spawn_blocking(move || {
        let root = mounts_root.display().to_string();
        let env = [("OAK_MOUNTS_ROOT", root.as_str())];
        let home = destination.parent().unwrap();
        (
            oak_env_with(
                &destination,
                home,
                &["desc", "mounted words", "--json"],
                None,
                &env,
            ),
            oak_env_with(&destination, home, &["status", "--json"], None, &env),
            oak_env_with(&destination, home, &["info", "--json"], None, &env),
        )
    })
    .await
    .unwrap();
    assert!(desc.status.success(), "{desc:?}");
    let value = json(&desc);
    assert_eq!(value["branch"], "receipt-test--12345678");
    assert_eq!(value["description"], "mounted words");
    assert_eq!(value["local_saved"], true);
    assert_eq!(value["remote_synced"], true);
    assert_eq!(value["remote_state"], "synced");

    let review = format!("{uri}/qa/widget/branches/receipt-test--12345678");
    for output in [status, info] {
        assert!(output.status.success(), "{output:?}");
        let value = json(&output);
        assert_eq!(value["web_url"], format!("{uri}/qa/widget"), "{value}");
        assert_eq!(value["review_url"], review.as_str(), "{value}");
        assert!(value["repository_root"].as_str().unwrap().ends_with("/mnt"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tokio::test]
async fn mount_desc_json_rejection_is_false_without_push_advice() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/api/qa/widget/push"))
        .respond_with(ResponseTemplate::new(409).set_body_string("PRIVATE_REMOTE_DIAGNOSTIC"))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().unwrap();
    let (destination, mounts_root) = fake_mount(home.path(), &server.uri());
    let output = tokio::task::spawn_blocking(move || {
        let root = mounts_root.display().to_string();
        let home = destination.parent().unwrap();
        oak_env_with(
            &destination,
            home,
            &["desc", "mounted words", "--json"],
            None,
            &[("OAK_MOUNTS_ROOT", root.as_str())],
        )
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    assert_eq!(value["remote_synced"], false);
    assert_eq!(value["retry_command"], "oak desc --json -- 'mounted words'");
    let everything = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!everything.contains("oak push"), "{everything}");
    assert!(
        !everything.contains("PRIVATE_REMOTE_DIAGNOSTIC"),
        "{everything}"
    );
}

#[tokio::test]
async fn repo_list_commands_keep_user_text_out_of_flag_position() {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "repos": [
                {"name": "--help", "owner": "-dash"},
                {"name": "two", "owner": "-dash"}
            ]
        })))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let uri = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run(&[
            "repo",
            "list",
            "--json",
            "--remote",
            &uri,
            "--org=-dash",
            "--limit",
            "1",
        ])
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json(&output);
    let next: Vec<&str> = value["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        next[0],
        format!(
            "oak repo list --json --remote={} --limit 2 --org=-dash",
            server.uri()
        )
    );
    assert_eq!(
        next[1],
        format!(
            "oak clone --shallow --remote={} -- -dash/--help",
            server.uri()
        )
    );
}

#[tokio::test]
async fn repo_list_transport_error_redacts_remote_userinfo() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let remote = format!("http://agent:s3cret-userinfo@127.0.0.1:{port}");
    let fixture = Fixture::new();
    let output = tokio::task::spawn_blocking(move || {
        fixture.run(&["repo", "list", "--json", "--remote", &remote])
    })
    .await
    .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let all = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!all.contains("s3cret-userinfo"), "{all}");
    assert!(all.contains(&format!("127.0.0.1:{port}")), "{all}");
}

#[test]
fn stored_remote_userinfo_is_never_printed() {
    let fixture = Fixture::new();
    fixture.link("https://agent:s3cret-stored@oak.example", "acme", "web");
    let branch = fixture.branch();
    for args in [
        vec!["status", "--json"],
        vec!["info", "--json"],
        vec!["branch", "show", branch.as_str(), "--json"],
        vec!["open", "--json"],
        vec!["open", "--print"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let all = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(!all.contains("s3cret-stored"), "{args:?}: {all}");
        assert!(
            all.contains("https://oak.example/acme/web"),
            "{args:?}: {all}"
        );
    }
}
