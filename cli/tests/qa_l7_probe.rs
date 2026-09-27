//! QA-L7 probes (review worker only; proposed acceptance tests, not pushed).

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use oak_cli::commands::credentials::{remote_origin, same_remote_origin, RepositoryCredential};
use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO_TOKEN: &str = "repo-key-SECRET-qa-l7";

fn oak_command(home: &Path, cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", " ")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env_remove("OAK_TRUSTED_REMOTES")
        .env("OAK_FEATURES", "all")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .kill_on_drop(true);
    command
}

async fn run(command: &mut tokio::process::Command) -> Output {
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("oak timed out")
        .expect("spawn oak")
}

/// Keyed checkout. `remote_url`: None = no RemoteUrl row.
async fn checkout(
    home: &Path,
    remote_url: Option<&str>,
    bound_origin: Option<&str>,
) -> std::path::PathBuf {
    let checkout = home.join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    assert!(run(oak_command(home, &checkout).args(["init", "."]))
        .await
        .status
        .success());
    std::fs::write(checkout.join("f.txt"), "x\n").unwrap();
    assert!(run(oak_command(home, &checkout).args(["commit"]))
        .await
        .status
        .success());
    let repo = SqliteRepository::open(&checkout.join(".oak/oak.db")).unwrap();
    if let Some(remote) = remote_url {
        repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    }
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
    match bound_origin {
        Some(origin) => {
            oak_cli::commands::credentials::store_repository_key(&repo, origin, REPO_TOKEN).unwrap()
        }
        None => repo.set_metadata(MetadataKey::ApiKey, REPO_TOKEN).unwrap(),
    }
    std::fs::create_dir_all(home.join(".oak")).unwrap();
    std::fs::write(home.join(".oak/credentials"), "[]").unwrap();
    checkout
}

async fn auth_headers(server: &MockServer) -> (usize, Vec<String>) {
    let requests = server.received_requests().await.unwrap();
    let auth = requests
        .iter()
        .filter_map(|r| r.headers.get("authorization"))
        .map(|v| v.to_str().unwrap_or("?").to_string())
        .collect();
    (requests.len(), auth)
}

fn transcript(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Lead check 1: key bound to A; `oak fetch -r <other>` where other requires
/// auth. Other must see no Authorization, the command must exit non-zero, and
/// must never claim success / linked ancestry.
#[tokio::test]
async fn fetch_remote_override_other_origin_sends_no_auth_and_fails() {
    for mode in ["401_everywhere", "head_then_401"] {
        let origin_a = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .mount(&origin_a)
            .await;
        let other = MockServer::start().await;
        if mode == "head_then_401" {
            Mock::given(method("GET"))
                .and(path("/api/oak/repo"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"head": "a".repeat(64), "name": "repo", "owner": "oak"}),
                ))
                .mount(&other)
                .await;
        }
        Mock::given(any())
            .respond_with(ResponseTemplate::new(401))
            .mount(&other)
            .await;
        let home = tempfile::tempdir().unwrap();
        let co = checkout(home.path(), Some(&origin_a.uri()), Some(&origin_a.uri())).await;
        let output = run(oak_command(home.path(), &co).args(["fetch", "-r", &other.uri()])).await;
        let (count, auth) = auth_headers(&other).await;
        assert!(count > 0, "{mode}: other never contacted");
        assert!(auth.is_empty(), "{mode}: other got {auth:?}");
        assert_eq!(
            auth_headers(&origin_a).await.0,
            0,
            "{mode}: origin A contacted"
        );
        assert!(
            !output.status.success(),
            "{mode}: exit 0: {}",
            transcript(&output)
        );
        let text = transcript(&output);
        assert!(!text.contains("linked"), "{mode}: {text}");
        assert!(!text.contains("Fetched main"), "{mode}: {text}");
        assert!(!text.contains(REPO_TOKEN), "{mode}: key echoed");
    }
}

/// Lead check 2: IPv6 literals and userinfo.
#[test]
fn origin_ipv6_and_userinfo() {
    assert_eq!(
        remote_origin("https://[::1]:8443").as_deref(),
        Some("https://[::1]:8443")
    );
    assert_eq!(
        remote_origin("https://[::1]").as_deref(),
        Some("https://[::1]")
    );
    assert_eq!(
        remote_origin("https://[::1]:443").as_deref(),
        Some("https://[::1]")
    );
    assert_eq!(
        remote_origin("https://[0:0:0:0:0:0:0:1]").as_deref(),
        Some("https://[::1]")
    );
    assert!(!same_remote_origin("https://[::1]:8443", "https://[::1]"));
    assert!(!same_remote_origin("http://[::1]", "https://[::1]"));
    // userinfo is not part of the origin, and never survives into it
    for u in ["https://user@oak.space", "https://user:pw@oak.space"] {
        let o = remote_origin(u).unwrap();
        assert_eq!(o, "https://oak.space");
        assert!(!o.contains("user") && !o.contains("pw"));
    }
    // userinfo cannot make a different host compare equal
    for evil in [
        "https://oak.space@evil.example",
        "https://oak.space:443@evil.example",
        "https://evil.example\\@oak.space",
        "https://evil.example#@oak.space",
        "https://evil.example?@oak.space",
    ] {
        assert!(!same_remote_origin(evil, "https://oak.space"), "{evil}");
        let cred = RepositoryCredential::bound("https://oak.space", "k").unwrap();
        assert_eq!(cred.token_for(evil), None, "{evil}");
    }
    // other spellings
    assert!(!same_remote_origin(
        "https://oak.space.",
        "https://oak.space"
    ));
    assert!(!same_remote_origin(
        "http://localhost:1",
        "http://127.0.0.1:1"
    ));
    assert!(same_remote_origin(
        "https://bücher.example",
        "https://xn--bcher-kva.example"
    ));
    let debug = format!(
        "{:?}",
        RepositoryCredential::bound("https://u:pw@oak.space", "tok-SECRET").unwrap()
    );
    assert!(
        !debug.contains("tok-SECRET") && !debug.contains("pw"),
        "{debug}"
    );
}

/// Finding: a legacy key whose checkout has no parseable RemoteUrl is
/// "never used" — until a retarget writes RemoteUrl, which then becomes the
/// binding. `push -r <other>` therefore binds the key to <other>, and the
/// next command sends it there.
#[tokio::test]
async fn legacy_key_without_parseable_binding_is_not_rebound_by_retarget() {
    for stored in [None, Some("oak.space")] {
        let other = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(401))
            .mount(&other)
            .await;
        let home = tempfile::tempdir().unwrap();
        let co = checkout(home.path(), stored, None).await;
        let _ =
            run(oak_command(home.path(), &co).args(["push", "-r", &other.uri(), "--json"])).await;
        let _ = run(oak_command(home.path(), &co).args(["pull"])).await;
        let (count, auth) = auth_headers(&other).await;
        assert!(count > 0);
        assert!(
            auth.is_empty(),
            "stored={stored:?}: key sent to retargeted origin: {auth:?}"
        );
    }
}
