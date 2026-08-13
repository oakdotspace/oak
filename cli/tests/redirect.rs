//! Pins the CLI's behavior when the remote host has moved (e.g.
//! oakvcs.com 302-redirecting to oak.space) and the formatting of
//! server-error messages.
//!
//! Background: reqwest's default redirect policy rewrites a redirected
//! POST into a GET, so `POST /push` against a moved host quietly became
//! `GET /push` on the new origin and failed with an opaque 405 whose
//! empty body rendered as `error: Server error:`. The API client now
//! refuses to follow redirects and reports the move instead.

use chrono::Utc;
use oak_core::{
    Branch, ChangeType, FileChange, FileMode, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn seed_one_commit(repo: &SqliteRepository, branch_name: &str) {
    seed_one_commit_with_content(repo, branch_name, b"hello world\n".to_vec());
}

fn seed_one_commit_with_content(repo: &SqliteRepository, branch_name: &str, content: Vec<u8>) {
    let branch = Branch::new(
        branch_name.to_string(),
        Some("Test branch".to_string()),
        Some("main".to_string()),
    );
    repo.store_branch(&branch).unwrap();
    repo.set_current_branch(branch_name).unwrap();

    let blob_hash = repo.put_blob(content).unwrap();
    let manifest_hash = repo
        .put_manifest(vec![ManifestEntry {
            path: "hello.txt".to_string(),
            blob_hash: blob_hash.clone(),
            mode: FileMode::Regular,
        }])
        .unwrap();
    let commit_hash = repo
        .put_commit(
            branch_name.to_string(),
            None,
            None,
            manifest_hash,
            "tester".to_string(),
            None,
            Utc::now(),
            vec![FileChange {
                path: "hello.txt".to_string(),
                change_type: ChangeType::Added,
                old_blob_hash: None,
                new_blob_hash: Some(blob_hash),
                old_path: None,
                old_mode: None,
                new_mode: Some(FileMode::Regular),
            }],
        )
        .unwrap();
    repo.set_branch_head(branch_name, &commit_hash).unwrap();
    repo.set_head(&commit_hash).unwrap();
}

async fn run_push(repo: &SqliteRepository, work_tree: &std::path::Path, remote: &str) -> String {
    oak_cli::commands::push::push_async(
        repo,
        work_tree,
        remote,
        "oak",
        "oak",
        Some("tester-a71b16"),
        false,
        None,
    )
    .await
    .expect_err("push against this server must fail")
    .to_string()
}

/// A 301/302 from the remote must surface as a "remote has moved" error —
/// never be silently followed (which would replay the POSTs as GETs on the
/// new origin).
#[tokio::test(flavor = "current_thread")]
async fn push_reports_remote_move_instead_of_following_redirect() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    // The old host answers every request with a 302 to the new origin.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", "https://oak.space/api/oak/oak"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // The CLI must bail before attempting to push anything.
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(302))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(302))
        .expect(0)
        .mount(&server)
        .await;

    let msg = run_push(&repo, temp.path(), &server.uri()).await;
    assert!(
        msg.contains("remote has moved to https://oak.space"),
        "expected move message, got: {msg}"
    );
    assert!(
        msg.contains("oak push -r https://oak.space"),
        "expected actionable -r hint, got: {msg}"
    );
}

/// Same protection on the blobs/check fallback: a redirect there is the
/// moved-host case, not an "old server without /blobs/check", so it must
/// be fatal rather than falling through to re-uploading everything.
#[tokio::test(flavor = "current_thread")]
async fn blobs_check_redirect_is_fatal_not_a_fallback() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    // The payload must exceed the small-push inline threshold: below it the
    // blobs/check round trip is skipped entirely and the redirect-is-fatal
    // path under test would never run.
    seed_one_commit_with_content(&repo, "tester-a71b16", vec![0xA5u8; 128 * 1024]);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-a71b16"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", "https://oak.space/api/oak/oak/blobs/check"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(302))
        .expect(0)
        .mount(&server)
        .await;

    let msg = run_push(&repo, temp.path(), &server.uri()).await;
    assert!(
        msg.contains("remote has moved to https://oak.space"),
        "expected move message, got: {msg}"
    );
}

/// A redirect with a relative Location isn't a host move; it must still
/// produce an error that names the status rather than an empty message.
#[tokio::test(flavor = "current_thread")]
async fn relative_redirect_reports_status_not_a_move() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/login"))
        .expect(1)
        .mount(&server)
        .await;

    let msg = run_push(&repo, temp.path(), &server.uri()).await;
    assert!(!msg.contains("remote has moved"), "got: {msg}");
    assert!(msg.contains("HTTP 302"), "expected status in: {msg}");
    assert!(msg.contains("/login"), "expected Location in: {msg}");
}

/// The concrete pre-fix repro: /push answering 405 with an empty body
/// rendered as `error: Server error:` with nothing after the colon. The
/// message must always name the HTTP status.
#[tokio::test(flavor = "current_thread")]
async fn server_error_with_empty_body_names_the_status() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/tester-a71b16"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(405))
        .expect(1)
        .mount(&server)
        .await;

    let msg = run_push(&repo, temp.path(), &server.uri()).await;
    assert!(msg.contains("HTTP 405"), "expected status in: {msg}");
    let after_colon = msg.strip_prefix("Server error:").unwrap_or(&msg).trim();
    assert!(
        !after_colon.is_empty(),
        "error must never be a bare 'Server error:', got: {msg}"
    );
}

/// `OAK_TRUSTED_REMOTES` is process-global, so tests that set it serialize
/// on this lock to keep parallel test threads from clobbering each other's
/// value. Hold the returned guard for as long as the test depends on the
/// env var. Async-aware so the guard can be held across the awaits that
/// exercise the env var (clippy::await_holding_lock forbids that for a
/// std MutexGuard).
static TRUSTED_REMOTES_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn set_trusted_remotes(origins: &[String]) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = TRUSTED_REMOTES_LOCK.lock().await;
    std::env::set_var("OAK_TRUSTED_REMOTES", origins.join(","));
    guard
}

/// A full working directory that `push::run` / `pull::run` can resolve:
/// `.oak/oak.db` with one seeded commit and the owner/name metadata that
/// skips the interactive first-push linking.
fn seed_run_workdir(branch_name: &str) -> (TempDir, SqliteRepository) {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&temp.path().join(".oak").join("oak.db")).unwrap();
    seed_one_commit(&repo, branch_name);
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    (temp, repo)
}

/// When the redirect target is a trusted Oak host, `oak push` and `oak pull`
/// must follow the move on their own: update the repo's stored remote and
/// retry once against the new origin, instead of telling the user to re-run
/// with `-r`. The trusted-host list is extended through the process-global
/// `OAK_TRUSTED_REMOTES` env var (a wiremock server can't listen on
/// https://oak.space), serialized across tests via `set_trusted_remotes`.
#[tokio::test(flavor = "current_thread")]
async fn push_and_pull_follow_move_to_trusted_origin() {
    let branch_name = "tester-a71b16";

    // --- push: old origin redirects, new origin serves a full push flow ---
    let (push_dir, push_repo) = seed_run_workdir(branch_name);
    let new_push = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&new_push)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/branches/{branch_name}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&new_push)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/blobs/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "missing": []
        })))
        .mount(&new_push)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        })))
        .expect(1)
        .mount(&new_push)
        .await;

    let old_push = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/api/oak/oak", new_push.uri()).as_str(),
        ))
        .expect(1)
        .mount(&old_push)
        .await;

    // --- pull: same shape; the retried pull also drives the parent-sync
    // phase, which re-reads the (now updated) stored remote ---
    let (pull_dir, pull_repo) = seed_run_workdir(branch_name);
    let new_pull = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&new_pull)
        .await;
    // Parent-sync resolves main's head via the repo info endpoint; a null
    // head makes it a no-op.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .mount(&new_pull)
        .await;

    let old_pull = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/api/oak/oak/pull", new_pull.uri()).as_str(),
        ))
        .expect(1)
        .mount(&old_pull)
        .await;

    let _env = set_trusted_remotes(&[new_push.uri(), new_pull.uri()]).await;

    oak_cli::commands::push::run(push_dir.path(), Some(&old_push.uri()), false, None)
        .await
        .expect("push must follow the trusted move and succeed");
    assert_eq!(
        push_repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(new_push.uri()),
        "push must persist the new origin as this repo's remote"
    );

    oak_cli::commands::pull::run(pull_dir.path(), Some(&old_pull.uri()), false, false, false)
        .await
        .expect("pull must follow the trusted move and succeed");
    assert_eq!(
        pull_repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(new_pull.uri()),
        "pull must persist the new origin as this repo's remote"
    );
}

/// A redirect to an origin that is NOT a trusted Oak host must never be
/// followed — the CLI keeps the actionable "re-run with -r" error so the
/// user makes the call about pointing the repo somewhere unexpected.
#[tokio::test(flavor = "current_thread")]
async fn push_does_not_follow_move_to_untrusted_origin() {
    let (dir, repo) = seed_run_workdir("tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", "https://evil.example/api/oak/oak"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let msg = oak_cli::commands::push::run(dir.path(), Some(&server.uri()), false, None)
        .await
        .expect_err("untrusted move must not be auto-followed")
        .to_string();
    assert!(
        msg.contains("remote has moved to https://evil.example"),
        "expected move message, got: {msg}"
    );
    assert!(
        msg.contains("oak push -r https://evil.example"),
        "expected actionable -r hint, got: {msg}"
    );
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(server.uri()),
        "the stored remote must not be retargeted at an untrusted origin"
    );
}

/// `oak fetch` reads its remote from the repo's stored `RemoteUrl` rather
/// than a flag, so it's the command an oakvcs.com-era clone is most likely
/// to hit a moved host with — it must follow a trusted move the same way
/// push/pull do. Also pins the sync-path fix: the parent-head GET used
/// `error_for_status`, which passes 3xx through, so the move used to
/// surface as a JSON decode error instead of `RemoteMoved`.
#[tokio::test(flavor = "current_thread")]
async fn fetch_follows_move_to_trusted_origin() {
    let (dir, repo) = seed_run_workdir("tester-a71b16");

    let new_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null
        })))
        .expect(1)
        .mount(&new_server)
        .await;

    let old_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/api/oak/oak", new_server.uri()).as_str(),
        ))
        .expect(1)
        .mount(&old_server)
        .await;

    // fetch takes no remote flag — it reads the stored RemoteUrl.
    repo.set_metadata(MetadataKey::RemoteUrl, &old_server.uri())
        .unwrap();

    let _env = set_trusted_remotes(&[new_server.uri()]).await;

    oak_cli::commands::fetch::run(dir.path(), None)
        .await
        .expect("fetch must follow the trusted move and succeed");
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(new_server.uri()),
        "fetch must persist the new origin as this repo's remote"
    );
}

/// `oak clone` against a moved host must retry against the trusted new
/// origin and record THAT origin as the fresh clone's remote. The first
/// attempt's cleanup removes the `.oak` dir it created, so the retry can
/// run from scratch without tripping the already-exists check.
#[tokio::test(flavor = "current_thread")]
async fn clone_follows_move_to_trusted_origin() {
    let new_server = MockServer::start().await;
    // An empty repo is the smallest valid clone: no commits, no blobs.
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "head": null,
            "branch": null,
            "branches": [],
            "commits": [],
            "blobs": [],
            "trees": []
        })))
        .expect(1)
        .mount(&new_server)
        .await;

    let old_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/pull"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/api/oak/oak/pull", new_server.uri()).as_str(),
        ))
        .expect(1)
        .mount(&old_server)
        .await;

    let _env = set_trusted_remotes(&[new_server.uri()]).await;

    let temp = TempDir::new().unwrap();
    let dest = temp.path().join("cloned");
    oak_cli::commands::repo::clone_repo(&old_server.uri(), "oak/oak", &dest, false)
        .await
        .expect("clone must follow the trusted move and succeed");

    let repo = SqliteRepository::open(&dest.join(".oak").join("oak.db")).unwrap();
    assert_eq!(
        repo.get_metadata(MetadataKey::RemoteUrl).unwrap(),
        Some(new_server.uri()),
        "the fresh clone must record the new origin as its remote"
    );
}

/// When the server does send an error body, it must appear alongside the
/// status.
#[tokio::test(flavor = "current_thread")]
async fn server_error_includes_status_and_body() {
    let temp = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
    seed_one_commit(&repo, "tester-a71b16");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak"))
        .respond_with(ResponseTemplate::new(500).set_body_string("database exploded"))
        .expect(1)
        .mount(&server)
        .await;

    let msg = run_push(&repo, temp.path(), &server.uri()).await;
    assert!(msg.contains("HTTP 500"), "expected status in: {msg}");
    assert!(msg.contains("database exploded"), "expected body in: {msg}");
}
