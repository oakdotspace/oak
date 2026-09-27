//! A checkout's repository API key is only valid for the origin that issued
//! it. Every command family that can be pointed at another server
//! (`OAK_REMOTE`, `-r/--remote`, a persisted `push -r`, a mount whose cache
//! was minted elsewhere, a cross-origin redirect) must reach that other
//! server without an `Authorization` header carrying this checkout's key,
//! while the issuing origin keeps authenticating.
//!
//! Each test runs the real binary in an isolated HOME with a blank
//! `OAK_API_KEY`, against wiremock servers that answer everything with a
//! non-success status; commands are expected to fail, the assertions are
//! about what the servers received.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO_TOKEN: &str = "repo-key-SECRET-issued-by-origin-a";
const ACCOUNT_TOKEN_A: &str = "account-token-for-origin-a";
const ACCOUNT_TOKEN_B: &str = "account-token-for-origin-b";

struct Fixture {
    home: tempfile::TempDir,
    checkout: PathBuf,
}

impl Fixture {
    fn db(&self) -> SqliteRepository {
        SqliteRepository::open(&self.checkout.join(".oak/oak.db")).unwrap()
    }

    /// `args` with `@branch` replaced by the checkout's current branch.
    fn args(&self, args: &[&str]) -> Vec<String> {
        let branch = self.db().get_current_branch_name().unwrap().unwrap();
        args.iter()
            .map(|arg| {
                if *arg == "@branch" {
                    branch.clone()
                } else {
                    arg.to_string()
                }
            })
            .collect()
    }
}

/// How the repository key was stored.
#[derive(Clone, Copy)]
enum KeyBinding {
    /// Written by a current client: `ApiKeyOrigin` recorded.
    Bound,
    /// Written by an older client: only `RemoteUrl` identifies the origin.
    Legacy,
}

fn write_credentials(home: &Path, entries: &[(&str, &str)]) {
    let oak_dir = home.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let rows: Vec<_> = entries
        .iter()
        .map(|(server, token)| {
            serde_json::json!({"server": server, "token": token, "username": "tester"})
        })
        .collect();
    std::fs::write(
        oak_dir.join("credentials"),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}

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
        .expect("command timed out")
        .unwrap()
}

/// A linked checkout at `origin_a` holding a repository key, one committed
/// file, and an account login for origin A only.
async fn fixture(origin_a: &str, binding: KeyBinding) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let checkout = home.path().join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let init = run(oak_command(home.path(), &checkout).args(["init", "."])).await;
    assert!(init.status.success(), "{init:?}");
    std::fs::write(checkout.join("file.txt"), "content\n").unwrap();
    let commit = run(oak_command(home.path(), &checkout).args(["commit"])).await;
    assert!(commit.status.success(), "{commit:?}");
    {
        let repo = SqliteRepository::open(&checkout.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, origin_a).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
        match binding {
            KeyBinding::Bound => {
                oak_cli::commands::credentials::store_repository_key(&repo, origin_a, REPO_TOKEN)
                    .unwrap()
            }
            KeyBinding::Legacy => repo.set_metadata(MetadataKey::ApiKey, REPO_TOKEN).unwrap(),
        }
    }
    write_credentials(home.path(), &[(origin_a, ACCOUNT_TOKEN_A)]);
    Fixture { home, checkout }
}

/// A server that answers every request with `status`.
async fn server_answering(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

/// A server that redirects every request to the same path on `target`.
async fn redirecting_server(target: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{target}/api/moved")),
        )
        .mount(&server)
        .await;
    server
}

async fn authorizations(server: &MockServer) -> (usize, Vec<String>) {
    let requests = server.received_requests().await.unwrap();
    let auth = requests
        .iter()
        .filter_map(|request| request.headers.get("authorization"))
        .map(|value| value.to_str().unwrap_or("<non-utf8>").to_string())
        .collect();
    (requests.len(), auth)
}

/// The other server was contacted (so the assertion is not vacuous) and
/// never saw any Authorization header.
async fn assert_contacted_without_auth(server: &MockServer, label: &str, output: &Output) {
    let (count, auth) = authorizations(server).await;
    assert!(
        count > 0,
        "{label}: the other server was never contacted, test is vacuous; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        auth.is_empty(),
        "{label}: another origin received Authorization {auth:?}"
    );
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!transcript.contains(REPO_TOKEN), "{label}: key echoed");
}

async fn assert_never_contacted(server: &MockServer, label: &str) {
    let (count, _) = authorizations(server).await;
    assert_eq!(count, 0, "{label}: redirect target was contacted");
}

async fn assert_authenticated_with(server: &MockServer, token: &str, label: &str, output: &Output) {
    let (count, auth) = authorizations(server).await;
    assert!(
        count > 0,
        "{label}: server never contacted; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = format!("Bearer {token}");
    assert!(
        auth.iter().any(|value| value == &expected),
        "{label}: expected {expected:?}, got {auth:?}"
    );
    assert!(
        auth.iter().all(|value| value == &expected),
        "{label}: unexpected credential among {auth:?}"
    );
}

// ---------------------------------------------------------------------------
// push

#[tokio::test]
async fn push_with_oak_remote_override_never_sends_repository_key() {
    for binding in [KeyBinding::Bound, KeyBinding::Legacy] {
        let origin_a = server_answering(404).await;
        let other = server_answering(404).await;
        let fx = fixture(&origin_a.uri(), binding).await;
        let output = run(oak_command(fx.home.path(), &fx.checkout)
            .env("OAK_REMOTE", other.uri())
            .args(["push", "--json"]))
        .await;
        assert_contacted_without_auth(&other, "OAK_REMOTE push", &output).await;
        assert_eq!(authorizations(&origin_a).await.0, 0);
    }
}

#[tokio::test]
async fn push_with_explicit_remote_never_sends_repository_key_and_pins_binding() {
    for binding in [KeyBinding::Bound, KeyBinding::Legacy] {
        let origin_a = server_answering(404).await;
        let other = server_answering(404).await;
        let fx = fixture(&origin_a.uri(), binding).await;
        let output = run(oak_command(fx.home.path(), &fx.checkout).args([
            "push",
            "-r",
            &other.uri(),
            "--json",
        ]))
        .await;
        assert_contacted_without_auth(&other, "push -r", &output).await;

        // `push -r` persists the new remote, but the key stays bound to the
        // origin that issued it (legacy checkouts get pinned before rewrite).
        let repo = fx.db();
        assert_eq!(
            repo.get_metadata(MetadataKey::RemoteUrl)
                .unwrap()
                .as_deref(),
            Some(other.uri().as_str())
        );
        assert_eq!(
            repo.get_metadata(MetadataKey::ApiKeyOrigin)
                .unwrap()
                .as_deref(),
            Some(origin_a.uri().as_str())
        );
    }
}

#[tokio::test]
async fn persisted_foreign_remote_never_receives_repository_key_from_any_family() {
    // The state a legacy `oak push -r <other>` leaves behind: RemoteUrl now
    // names another server while the key was issued by origin A.
    let origin_a = server_answering(404).await;
    let fx = fixture(&origin_a.uri(), KeyBinding::Bound).await;
    let families: &[(&str, &[&str])] = &[
        ("push", &["push", "--json"]),
        ("pull", &["pull"]),
        ("fetch", &["fetch"]),
        ("desc sync", &["desc", "new description"]),
        ("ci runs", &["ci", "runs", "--json"]),
        ("ci status", &["ci", "status", "--json"]),
        (
            "branch show --remote",
            &["branch", "show", "@branch", "--remote", "--json"],
        ),
        ("release list", &["release", "list"]),
        ("merge", &["merge", "--json"]),
        ("finish", &["finish", "--desc", "done", "--json"]),
    ];
    for (label, args) in families {
        let other = server_answering(404).await;
        fx.db()
            .set_metadata(MetadataKey::RemoteUrl, &other.uri())
            .unwrap();
        let output = run(oak_command(fx.home.path(), &fx.checkout).args(fx.args(args))).await;
        // finish has no credential for the effective remote (the key does
        // not count). Because the mock is loopback and does not answer 401,
        // finish's open-loopback-Serve exception lets it proceed anonymously;
        // either way it must never present the key.
        assert_contacted_without_auth(&other, label, &output).await;
        if *label == "finish" {
            assert!(!output.status.success(), "finish succeeded anonymously");
        }
    }
    assert_eq!(authorizations(&origin_a).await.0, 0);
}

#[tokio::test]
async fn foreign_remote_uses_its_own_account_login_not_the_repository_key() {
    let origin_a = server_answering(404).await;
    let other = server_answering(404).await;
    let fx = fixture(&origin_a.uri(), KeyBinding::Bound).await;
    write_credentials(
        fx.home.path(),
        &[
            (&origin_a.uri(), ACCOUNT_TOKEN_A),
            (&other.uri(), ACCOUNT_TOKEN_B),
        ],
    );
    let output = run(oak_command(fx.home.path(), &fx.checkout)
        .env("OAK_REMOTE", other.uri())
        .args(["push", "--json"]))
    .await;
    assert_authenticated_with(&other, ACCOUNT_TOKEN_B, "OAK_REMOTE push w/ login", &output).await;
}

#[tokio::test]
async fn issuing_origin_still_receives_repository_key() {
    for binding in [KeyBinding::Bound, KeyBinding::Legacy] {
        let families: &[(&str, &[&str])] = &[
            ("push", &["push", "--json"]),
            ("fetch", &["fetch"]),
            ("ci runs", &["ci", "runs", "--json"]),
            ("desc sync", &["desc", "new description"]),
        ];
        for (label, args) in families {
            let origin_a = server_answering(404).await;
            let fx = fixture(&origin_a.uri(), binding).await;
            let output = run(oak_command(fx.home.path(), &fx.checkout).args(*args)).await;
            assert_authenticated_with(&origin_a, REPO_TOKEN, label, &output).await;
        }
    }
}

#[tokio::test]
async fn origin_comparison_is_normalized_not_textual() {
    // Same origin spelled with a trailing slash and upper-case scheme/host:
    // still the issuing origin.
    let origin_a = server_answering(404).await;
    let fx = fixture(&origin_a.uri(), KeyBinding::Bound).await;
    let spelled = format!(
        "{}/",
        origin_a
            .uri()
            .replace("http://127.0.0.1", "HTTP://127.0.0.1")
    );
    let output = run(oak_command(fx.home.path(), &fx.checkout)
        .env("OAK_REMOTE", &spelled)
        .args(["fetch"]))
    .await;
    assert_authenticated_with(&origin_a, REPO_TOKEN, "normalized fetch", &output).await;
}

// ---------------------------------------------------------------------------
// pull / fetch

#[tokio::test]
async fn pull_and_fetch_remote_overrides_never_send_repository_key() {
    for binding in [KeyBinding::Bound, KeyBinding::Legacy] {
        let cases: &[(&str, bool, &[&str])] = &[
            ("pull -r", false, &["pull", "-r"]),
            ("fetch -r", false, &["fetch", "-r"]),
            ("OAK_REMOTE pull", true, &["pull"]),
            ("OAK_REMOTE fetch", true, &["fetch"]),
        ];
        for (label, via_env, args) in cases {
            let origin_a = server_answering(404).await;
            let other = server_answering(404).await;
            let fx = fixture(&origin_a.uri(), binding).await;
            let mut command = oak_command(fx.home.path(), &fx.checkout);
            command.args(*args);
            if *via_env {
                command.env("OAK_REMOTE", other.uri());
            } else {
                command.arg(other.uri());
            }
            let output = run(&mut command).await;
            assert_contacted_without_auth(&other, label, &output).await;
            // The anonymous call to the other origin must not look like success.
            assert!(
                !output.status.success(),
                "{label}: exited 0 against an origin that rejected everything"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// account-scoped families invoked from inside a keyed checkout

#[tokio::test]
async fn remote_flag_families_inside_checkout_never_send_repository_key() {
    let cases: &[(&str, &[&str])] = &[
        (
            "feedback --remote",
            &["feedback", "-m", "hello", "--remote"],
        ),
        ("site show --remote", &["site", "show", "--remote"]),
        ("clone -r", &["clone", "oak/other", "cloned", "-r"]),
    ];
    for (label, args) in cases {
        let origin_a = server_answering(404).await;
        let other = server_answering(404).await;
        let fx = fixture(&origin_a.uri(), KeyBinding::Bound).await;
        let output = run(oak_command(fx.home.path(), &fx.checkout)
            .args(*args)
            .arg(other.uri()))
        .await;
        assert_contacted_without_auth(&other, label, &output).await;
    }
}

// ---------------------------------------------------------------------------
// redirects

#[tokio::test]
async fn authenticated_requests_never_follow_cross_origin_redirects() {
    let families: &[(&str, &[&str])] = &[
        ("push", &["push", "--json"]),
        ("pull", &["pull"]),
        ("fetch", &["fetch"]),
        ("desc sync", &["desc", "new description"]),
        ("ci runs", &["ci", "runs", "--json"]),
        (
            "branch show --remote",
            &["branch", "show", "@branch", "--remote", "--json"],
        ),
        ("release list", &["release", "list"]),
    ];
    for (label, args) in families {
        let target = server_answering(200).await;
        let origin_a = redirecting_server(&target.uri()).await;
        let fx = fixture(&origin_a.uri(), KeyBinding::Bound).await;
        let output = run(oak_command(fx.home.path(), &fx.checkout).args(fx.args(args))).await;
        // The issuing origin authenticated; the redirect was not followed.
        assert_authenticated_with(&origin_a, REPO_TOKEN, label, &output).await;
        assert_never_contacted(&target, label).await;
    }
}

// ---------------------------------------------------------------------------
// mounts

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tokio::test]
async fn mount_never_sends_cache_key_to_a_different_origin() {
    use oak_cli::commands::mount;
    use oak_core::Branch;
    use std::collections::HashMap;

    let origin_a = server_answering(404).await;
    let other = server_answering(404).await;
    let home = tempfile::tempdir().unwrap();
    write_credentials(home.path(), &[(&origin_a.uri(), ACCOUNT_TOKEN_A)]);

    let destination = home.path().join("mount");
    std::fs::create_dir_all(&destination).unwrap();
    let mounts_root = home.path().join(".oak/mounts");
    let id = "credential-scope";
    let state_dir = mounts_root.join(id);
    std::fs::create_dir_all(&state_dir).unwrap();
    let virtual_branch = "credential-scope--12345678";
    let cache = SqliteRepository::open_relaxed(&mount::state::cache_db_path(&state_dir)).unwrap();
    oak_cli::commands::credentials::store_repository_key(&cache, &origin_a.uri(), REPO_TOKEN)
        .unwrap();
    cache
        .store_branch(&Branch::new(
            virtual_branch.to_string(),
            Some("before".to_string()),
            Some("main".to_string()),
        ))
        .unwrap();
    // The mount itself points at another server.
    mount::state::save_config(
        &state_dir,
        &mount::state::MountConfig {
            id: id.to_string(),
            mount_point: destination.clone(),
            remote_url: other.uri(),
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

    let output = run(oak_command(home.path(), &destination)
        .env("OAK_MOUNTS_ROOT", &mounts_root)
        .args(["desc", "mounted description"]))
    .await;
    assert_contacted_without_auth(&other, "mount desc", &output).await;
    assert_eq!(authorizations(&origin_a).await.0, 0);
}

// ---------------------------------------------------------------------------
// auth status (reports the credential it would use, then probes /api/whoami)

fn auth_source(output: &Output) -> String {
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "auth status JSON: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    json["credential"]["source"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn auth_status_follows_origin_binding_after_retarget() {
    for binding in [KeyBinding::Bound, KeyBinding::Legacy] {
        let origin_a = server_answering(404).await;
        let other = server_answering(401).await;
        let fx = fixture(&origin_a.uri(), binding).await;

        // Issuing origin: the repository key is reported and sent.
        let output =
            run(oak_command(fx.home.path(), &fx.checkout).args(["auth", "status", "--json"])).await;
        assert_eq!(auth_source(&output), "repository");
        assert_authenticated_with(&origin_a, REPO_TOKEN, "auth status (own origin)", &output).await;

        // `push -r B` persists B as the stored remote; the key stays bound
        // to A, so auth status must neither send it to B nor claim it.
        let _ = run(oak_command(fx.home.path(), &fx.checkout).args([
            "push",
            "-r",
            &other.uri(),
            "--json",
        ]))
        .await;
        other.reset().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(401))
            .mount(&other)
            .await;
        let output =
            run(oak_command(fx.home.path(), &fx.checkout).args(["auth", "status", "--json"])).await;
        assert_contacted_without_auth(&other, "auth status after push -r", &output).await;
        assert_eq!(auth_source(&output), "none");
    }
}

#[tokio::test]
async fn auth_status_never_sends_an_unbound_legacy_key() {
    for stored in [None, Some("oak.space")] {
        let origin_a = server_answering(404).await;
        let other = server_answering(401).await;
        let fx = fixture(&origin_a.uri(), KeyBinding::Legacy).await;
        {
            let repo = fx.db();
            match stored {
                // Older `oak create` stored an un-normalized OAK_REMOTE.
                Some(raw) => repo.set_metadata(MetadataKey::RemoteUrl, raw).unwrap(),
                None => {
                    // Remove the RemoteUrl row by overwriting with the
                    // un-parseable empty value older clients could leave.
                    repo.set_metadata(MetadataKey::RemoteUrl, "").unwrap()
                }
            }
        }
        // Retarget pins the key as unbound and stores B.
        let _ = run(oak_command(fx.home.path(), &fx.checkout).args([
            "push",
            "-r",
            &other.uri(),
            "--json",
        ]))
        .await;
        assert_eq!(
            fx.db()
                .get_metadata(MetadataKey::ApiKeyOrigin)
                .unwrap()
                .as_deref(),
            Some(oak_cli::commands::credentials::UNBOUND_REPOSITORY_KEY_ORIGIN)
        );
        other.reset().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(401))
            .mount(&other)
            .await;
        let output =
            run(oak_command(fx.home.path(), &fx.checkout).args(["auth", "status", "--json"])).await;
        assert_contacted_without_auth(&other, "auth status (unbound key)", &output).await;
        assert_eq!(auth_source(&output), "none");
        assert_eq!(authorizations(&origin_a).await.0, 0);
    }
}
