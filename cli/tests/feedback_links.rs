//! `oak feedback link` / `oak feedback unlink` — the CLI surface over the
//! admin-only feedback-links API.
//!
//! These endpoints are not deployed, so nothing here talks to a real server
//! and none of it is evidence that the production API behaves this way. What
//! it *does* prove is everything the CLI controls: the exact request it
//! constructs, the credential it is willing to attach and to whom, how each
//! failure reads, and what it exits with.
//!
//! Every probe points at a `127.0.0.1` wiremock server — `oak feedback`'s
//! submit path is rate-limited and its queue is real, so no test may send
//! anything to oak.space.
//!
//! The commands run as subprocesses rather than as library calls. That costs
//! a process spawn per case and buys three things a direct call cannot give:
//! real clap parsing (so flag *placement* is under test), real exit codes,
//! and a private `HOME` and environment per case — which is what lets the
//! credential-isolation tests below be meaningful.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{Branch, MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::{body_json, body_partial_json, method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The 28-hex feedback id `fb-165` resolves to.
const ITEM_ID: &str = "0123456789abcdef0123456789ab";

/// The token the *checkout's* origin issued. It must never leave for any
/// other host; several tests below grep the wire for exactly this string.
const CHECKOUT_KEY: &str = "checkout-key-for-the-origin-only";

// --- harness ---------------------------------------------------------------

/// Run `oak` in `dir` with a private `HOME`, no ambient Oak credentials, and
/// no update check. Returns the finished process, never panicking on a
/// non-zero exit — failure paths are the point of half these tests.
fn oak(dir: &Path, home: &Path, args: &[&str]) -> Output {
    oak_env(dir, home, args, &[])
}

fn oak_env(dir: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_EMAIL")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("oak binary should run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .expect("oak should exit, not be signalled")
}

/// A minimal on-disk repo `resolve::resolve` can find, on branch
/// `tester-fb`, linked to `remote` as `oak/oak` — **and carrying a
/// repository `ApiKey`**. That key is deliberate: the link/unlink path must
/// never reach for it, and it can only be proven not to if it is there.
fn fixture_repo(dir: &Path, remote: &str) {
    let oak_dir = dir.join(".oak");
    std::fs::create_dir_all(&oak_dir).unwrap();
    let repo = SqliteRepository::open(&oak_dir.join("oak.db")).unwrap();
    repo.store_branch(&Branch::new(
        "tester-fb".to_string(),
        None,
        Some("main".to_string()),
    ))
    .unwrap();
    repo.set_current_branch("tester-fb").unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    repo.set_metadata(MetadataKey::ApiKey, CHECKOUT_KEY)
        .unwrap();
}

/// Write `~/.oak/credentials` for `server` — what `oak login --remote
/// <server>` would have left behind.
fn login(home: &Path, server: &str, token: &str) {
    let dir = home.join(".oak");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("credentials"),
        serde_json::json!([{"server": server, "token": token, "username": "tester"}]).to_string(),
    )
    .unwrap();
}

/// A checkout plus a logged-in home for one server — the ordinary setup.
fn scene(server: &MockServer) -> (tempfile::TempDir, tempfile::TempDir) {
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), &server.uri());
    login(home.path(), &server.uri(), "admin-token-for-this-server");
    (work, home)
}

/// The `GET /api/feedback?status=all` payload. Rows carry a `links` array
/// the CLI ignores while resolving `fb-N`, and one row is spam-marked — the
/// server hides those from the default export, so an item only appears here
/// at all because the CLI asked for every status.
fn feedback_list() -> serde_json::Value {
    serde_json::json!([
        {"id": "aaaabbbbccccddddeeeeffff0000", "number": 164, "ref": "fb-164", "links": []},
        {"id": ITEM_ID, "number": 165, "ref": "fb-165", "title": "link feedback to branches",
         "status": "spam", "links": []},
    ])
}

/// Mount the `fb-N` lookup — and require `status=all` on it. Every test that
/// names an item by number therefore also asserts that query, because a
/// request without it matches no mock and the command fails.
async fn mount_lookup(server: &MockServer, times: u64) {
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .and(query_param("status", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feedback_list()))
        .expect(times)
        .mount(server)
        .await;
}

fn link_json(id: &str, branch: Option<&str>, commit: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "feedback_id": ITEM_ID,
        "repo_owner": "oak",
        "repo_name": "oak",
        "branch": branch,
        "commit_hash": commit,
        "link_type": if branch.is_some() { "branch" } else { "commit" },
        "created_at": "2026-07-31T12:00:00Z",
        "created_by": "tester",
    })
}

/// Mount `GET /api/feedback/{id}/links` returning exactly these links.
async fn mount_links(server: &MockServer, links: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(links)))
        .mount(server)
        .await;
}

async fn mount_delete(server: &MockServer, link_id: &str, times: u64) {
    Mock::given(method("DELETE"))
        .and(path_matcher(format!(
            "/api/feedback/{ITEM_ID}/links/{link_id}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "ok"})))
        .expect(times)
        .mount(server)
        .await;
}

/// Every `Authorization` header value the server was sent, across all
/// requests — the wire, as the server saw it.
async fn authorizations(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|req| req.headers.get("authorization").cloned())
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect()
}

/// A 127.0.0.1 port with nothing listening on it — a connection there fails
/// at the transport, without any risk of reaching a real host.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// --- link: every request shape ---------------------------------------------

#[tokio::test]
async fn link_defaults_repo_and_branch_to_the_current_checkout() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .and(body_json(serde_json::json!({
            "repo_owner": "oak",
            "repo_name": "oak",
            "branch": "tester-fb",
            "link_type": "branch",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-1",
            Some("tester-fb"),
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-165"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "↟ fb-165 linked to oak/oak@tester-fb.");
}

#[tokio::test]
async fn link_by_raw_id_skips_the_lookup() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 0).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-9",
            Some("other"),
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", ITEM_ID, "--branch", "other"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("↟ {ITEM_ID} linked to oak/oak@other.")
    );
}

#[tokio::test]
async fn link_with_commit_only_files_a_commit_link() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    // Exact body: `--commit` alone must not pick up the current branch.
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .and(body_json(serde_json::json!({
            "repo_owner": "acme",
            "repo_name": "widgets",
            "commit_hash": "ec1d378a00",
            "link_type": "commit",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-2",
            None,
            Some("ec1d378a00"),
        )))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "link",
            "fb-165",
            "--commit",
            "ec1d378a00",
            "--repo",
            "acme/widgets",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "↟ fb-165 linked to acme/widgets commit ec1d378a00."
    );
}

#[tokio::test]
async fn link_with_branch_and_commit_records_both() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .and(body_json(serde_json::json!({
            "repo_owner": "oak",
            "repo_name": "oak",
            "branch": "tester-fb",
            "commit_hash": "ec1d378a00",
            "link_type": "branch",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-3",
            Some("tester-fb"),
            Some("ec1d378a00"),
        )))
        .expect(1)
        .mount(&server)
        .await;

    // `--commit` alone would file a commit-only link; naming the branch too
    // is what records both on one `branch` link.
    let out = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "link",
            "fb-165",
            "--branch",
            "tester-fb",
            "--commit",
            "ec1d378a00",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "↟ fb-165 linked to oak/oak@tester-fb (ec1d378a00)."
    );
}

#[tokio::test]
async fn explicit_repo_flag_overrides_the_checkout() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .and(body_partial_json(serde_json::json!({
            "repo_owner": "acme",
            "repo_name": "widgets",
            "branch": "release",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-4",
            Some("release"),
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "link",
            "fb-165",
            "--repo",
            "acme/widgets",
            "--branch",
            "release",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "↟ fb-165 linked to acme/widgets@release.");
}

/// The lookup asks for `?status=all`, so an item the server marks as spam is
/// still addressable by an administrator. Only a `status=all` request
/// matches the mounted mock; anything else 404s and the command fails.
#[tokio::test]
async fn a_spam_marked_item_is_still_reachable_by_number() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .and(query_param("status", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feedback_list()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-1",
            Some("tester-fb"),
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-165"]);
    assert_eq!(
        code(&out),
        0,
        "fb-165 is spam-marked; without status=all it would be unaddressable: {}",
        stderr(&out)
    );
}

// --- --json envelopes -------------------------------------------------------

#[tokio::test]
async fn link_json_is_self_describing_and_its_undo_command_is_runnable() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 2).await;
    let mut created = link_json("lnk-1", Some("tester-fb"), None);
    created["source"] = serde_json::json!("manual");
    created["a_field_this_cli_predates"] = serde_json::json!("kept");
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(created))
        .mount(&server)
        .await;
    mount_links(
        &server,
        vec![
            link_json("lnk-1", Some("tester-fb"), None),
            link_json("lnk-2", Some("tester-fb"), None),
        ],
    )
    .await;
    mount_delete(&server, "lnk-1", 1).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["feedback_id"], ITEM_ID);
    assert_eq!(payload["feedback_ref"], "fb-165");
    assert_eq!(payload["link"]["link_type"], "branch");
    assert_eq!(
        payload["link"]["source"], "manual",
        "the server's `source` is surfaced verbatim"
    );
    assert_eq!(
        payload["link"]["a_field_this_cli_predates"], "kept",
        "unknown server fields pass through untouched"
    );

    // The promise is not that the string looks right — it is that running it
    // undoes the link. Note the item now has *two* links to that same
    // branch, so a repo+branch undo would be ambiguous; --link-id is not.
    let undo = payload["recommended_next_commands"][0].as_str().unwrap();
    assert_eq!(
        undo,
        format!(
            "oak feedback unlink fb-165 --link-id lnk-1 --remote {}",
            server.uri()
        )
    );
    let args: Vec<&str> = undo.split_whitespace().skip(1).collect();
    let replay = oak(work.path(), home.path(), &args);
    assert_eq!(
        code(&replay),
        0,
        "the undo command must work verbatim: {}",
        stderr(&replay)
    );
    assert_eq!(stdout(&replay), "↟ fb-165 unlinked from link lnk-1.");
}

#[tokio::test]
async fn unlink_json_reports_the_link_it_removed() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(&server, vec![link_json("lnk-1", Some("tester-fb"), None)]).await;
    mount_delete(&server, "lnk-1", 1).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["removed_link_id"], "lnk-1");
    assert_eq!(payload["removed"]["branch"], "tester-fb");
    assert_eq!(
        payload["recommended_next_commands"][0],
        format!(
            "oak feedback link fb-165 --repo oak/oak --branch tester-fb --remote {}",
            server.uri()
        )
    );
}

// --- argument placement -----------------------------------------------------

/// `--remote` and `--json` are declared on both the parent and the
/// subcommand and combined at dispatch, so where they are typed cannot
/// change what happens.
#[tokio::test]
async fn remote_and_json_are_identical_before_or_after_the_subcommand() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    // No checkout remote at all: --remote is the only thing pointing at the
    // server, so a dropped flag cannot accidentally still work.
    fixture_repo(work.path(), "https://unused.example");
    login(home.path(), &server.uri(), "admin-token-for-this-server");
    mount_lookup(&server, 2).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-1",
            Some("tester-fb"),
            None,
        )))
        .expect(2)
        .mount(&server)
        .await;

    let uri = server.uri();
    let before = oak(
        work.path(),
        home.path(),
        &["feedback", "--json", "--remote", &uri, "link", "fb-165"],
    );
    let after = oak(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--json", "--remote", &uri],
    );

    assert_eq!(code(&before), 0, "{}", stderr(&before));
    assert_eq!(code(&after), 0, "{}", stderr(&after));
    assert_eq!(
        stdout(&before),
        stdout(&after),
        "flag placement must not change the output"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout(&before)).unwrap()["schema_version"],
        1
    );
}

#[tokio::test]
async fn unlink_remote_and_json_are_identical_before_or_after_the_subcommand() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), "https://unused.example");
    login(home.path(), &server.uri(), "admin-token-for-this-server");
    mount_lookup(&server, 2).await;
    mount_links(&server, vec![link_json("lnk-1", Some("tester-fb"), None)]).await;
    mount_delete(&server, "lnk-1", 2).await;

    let uri = server.uri();
    let before = oak(
        work.path(),
        home.path(),
        &["feedback", "--json", "--remote", &uri, "unlink", "fb-165"],
    );
    let after = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--json", "--remote", &uri],
    );
    assert_eq!(code(&before), 0, "{}", stderr(&before));
    assert_eq!(code(&after), 0, "{}", stderr(&after));
    assert_eq!(stdout(&before), stdout(&after));
}

/// The submit path is load-bearing: every AGENTS.md in every Oak space tells
/// agents to run `oak feedback -m "..."`. Adding subcommands must not have
/// changed one byte of what that sends.
#[tokio::test]
async fn bare_feedback_still_submits_unchanged() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    Mock::given(method("POST"))
        .and(path_matcher("/api/feature-requests/cli"))
        .and(body_partial_json(serde_json::json!({
            "body": "the editor flow is great",
            "name": "tester",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "abc", "ref": "fb-900", "status": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "-m",
            "the editor flow is great",
            "--remote",
            &server.uri(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "↟ feedback sent — filed as fb-900. Thank you!"
    );
}

#[tokio::test]
async fn the_feature_request_alias_and_json_flag_still_work() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    Mock::given(method("POST"))
        .and(path_matcher("/api/feature-requests/cli"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "abc", "ref": "fb-901", "status": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &[
            "feature-request",
            "-m",
            "dark mode",
            "--json",
            "--remote",
            &server.uri(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let payload: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(payload["ref"], "fb-901");
}

/// The other half of the old contract: a non-interactive caller with no
/// message exits 2 rather than blocking on an editor.
#[tokio::test]
async fn bare_feedback_without_a_message_still_exits_two() {
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let out = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "--remote",
            &format!("http://127.0.0.1:{}", dead_port()),
        ],
    );
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("No message provided"),
        "{}",
        stderr(&out)
    );
}

// --- unlink: every selector -------------------------------------------------

#[tokio::test]
async fn unlink_by_link_id_deletes_exactly_that_row() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    // Two identical branch links: only the named id may go.
    mount_links(
        &server,
        vec![
            link_json("lnk-1", Some("tester-fb"), None),
            link_json("lnk-2", Some("tester-fb"), None),
        ],
    )
    .await;
    mount_delete(&server, "lnk-2", 1).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--link-id", "lnk-2"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "↟ fb-165 unlinked from link lnk-2.");
}

#[tokio::test]
async fn unlink_by_commit_reaches_a_commit_only_link() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(
        &server,
        vec![
            link_json("lnk-1", Some("tester-fb"), None),
            link_json("lnk-2", None, Some("ec1d378a00")),
        ],
    )
    .await;
    mount_delete(&server, "lnk-2", 1).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--commit", "ec1d378a00"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "↟ fb-165 unlinked from oak/oak commit ec1d378a00."
    );
}

#[tokio::test]
async fn unlink_by_commit_also_reaches_a_branch_plus_commit_link() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(
        &server,
        vec![
            link_json("lnk-1", Some("other"), None),
            link_json("lnk-2", Some("tester-fb"), Some("ec1d378a00")),
        ],
    )
    .await;
    mount_delete(&server, "lnk-2", 1).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--commit", "EC1D378A00"],
    );
    assert_eq!(
        code(&out),
        0,
        "a branch link filed with a commit must still be reachable by that commit: {}",
        stderr(&out)
    );
}

#[tokio::test]
async fn unlink_by_branch_resolves_the_link_id() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(
        &server,
        vec![
            link_json("lnk-7", Some("some-other-branch"), None),
            link_json("lnk-1", Some("tester-fb"), None),
        ],
    )
    .await;
    mount_delete(&server, "lnk-1", 1).await;

    let out = oak(work.path(), home.path(), &["feedback", "unlink", "fb-165"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "↟ fb-165 unlinked from oak/oak@tester-fb.");
}

/// Ambiguity is a dead end only if the error is a dead end. It must name the
/// candidates' link ids and hand back commands that resolve them — and it
/// must delete nothing in the meantime.
#[tokio::test]
async fn unlink_ambiguity_lists_link_ids_and_runnable_commands() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 2).await;
    mount_links(
        &server,
        vec![
            link_json("lnk-1", Some("tester-fb"), None),
            link_json("lnk-2", Some("tester-fb"), Some("ec1d378a00")),
        ],
    )
    .await;
    mount_delete(&server, "lnk-2", 1).await;

    let out = oak(work.path(), home.path(), &["feedback", "unlink", "fb-165"]);
    assert_eq!(code(&out), 1, "ambiguity must not be guessed at");
    let err = stderr(&out);
    assert!(err.contains("has 2 links to oak/oak@tester-fb"), "{err}");
    assert!(err.contains("lnk-1"), "{err}");
    assert!(err.contains("lnk-2"), "{err}");
    assert!(
        err.contains("oak feedback unlink fb-165 --link-id lnk-2"),
        "the way out must be spelled as a command: {err}"
    );
    assert!(stdout(&out).is_empty(), "--json stdout stays clean");

    // And the offered command actually resolves it (the DELETE mock above
    // expects exactly this one call).
    let replay = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--link-id", "lnk-2"],
    );
    assert_eq!(code(&replay), 0, "{}", stderr(&replay));
}

#[tokio::test]
async fn unlink_with_no_match_names_what_the_item_is_linked_to() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(
        &server,
        vec![link_json("lnk-7", Some("other-branch"), None)],
    )
    .await;
    mount_delete(&server, "lnk-7", 0).await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--branch", "nope"],
    );
    assert_eq!(code(&out), 1);
    let err = stderr(&out);
    assert!(err.contains("has no link to oak/oak@nope"), "{err}");
    assert!(err.contains("It is linked to:"), "{err}");
    assert!(err.contains("other-branch"), "{err}");
}

#[tokio::test]
async fn unlink_with_an_unknown_link_id_deletes_nothing() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;
    mount_links(&server, vec![link_json("lnk-1", Some("tester-fb"), None)]).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "unlink", "fb-165", "--link-id", "lnk-404"],
    );
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("has no link to link lnk-404"),
        "{}",
        stderr(&out)
    );
}

/// The three selectors are mutually exclusive — clap rejects the
/// combination before anything is sent.
#[tokio::test]
async fn unlink_rejects_two_selectors_at_once() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);

    for args in [
        vec![
            "feedback",
            "unlink",
            "fb-165",
            "--link-id",
            "lnk-1",
            "--branch",
            "x",
        ],
        vec![
            "feedback",
            "unlink",
            "fb-165",
            "--link-id",
            "lnk-1",
            "--commit",
            "abc",
        ],
        vec![
            "feedback", "unlink", "fb-165", "--branch", "x", "--commit", "abc",
        ],
    ] {
        let out = oak(work.path(), home.path(), &args);
        assert_ne!(code(&out), 0, "{args:?} should be rejected");
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "a rejected invocation must not talk to the server"
    );
}

// --- failure paths ----------------------------------------------------------

#[tokio::test]
async fn a_non_admin_404_reads_as_not_authorized() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    // The links API answers everyone who is not an admin with a quiet 404.
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-165"]);
    assert_eq!(code(&out), 1);
    let err = stderr(&out);
    assert!(err.contains("not authorized (or unknown item)"), "{err}");
    assert!(err.contains("admin-only"), "{err}");
}

#[tokio::test]
async fn an_unknown_item_number_says_which_number() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    mount_lookup(&server, 1).await;

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-999"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("no feedback item fb-999"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn a_transport_failure_names_the_remote() {
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let remote = format!("http://127.0.0.1:{}", dead_port());
    fixture_repo(work.path(), &remote);
    login(home.path(), &remote, "admin-token-for-this-server");

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-165"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains(&format!("could not reach {remote}")),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
async fn a_garbled_response_body_is_reported_not_panicked_on() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
        .mount(&server)
        .await;

    let out = oak(work.path(), home.path(), &["feedback", "link", "fb-165"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("could not parse the server's response"),
        "{}",
        stderr(&out)
    );
}

// --- credential isolation ---------------------------------------------------

/// With no credential for the effective remote, both commands fail *before*
/// opening a connection — the server sees nothing at all.
#[tokio::test]
async fn a_missing_credential_fails_before_any_request_is_sent() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), &server.uri());
    // No ~/.oak/credentials, no OAK_API_KEY — only the checkout's ApiKey,
    // which this path is not allowed to use.

    for verb in ["link", "unlink"] {
        let out = oak(work.path(), home.path(), &["feedback", verb, "fb-165"]);
        assert_eq!(code(&out), 2, "{verb}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("no credential for"), "{verb}: {err}");
        assert!(err.contains("oak login"), "{verb}: {err}");
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing may be sent before a credential for this remote is in hand"
    );
}

/// The positive control for the test below: when a credential *is* stored
/// for the remote being called, that is the token that goes out.
#[tokio::test]
async fn the_effective_remotes_own_credential_is_the_one_sent() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), "https://origin.example");
    login(home.path(), &server.uri(), "token-for-this-server");
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-1",
            Some("tester-fb"),
            None,
        )))
        .mount(&server)
        .await;

    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--remote", &server.uri()],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let sent = authorizations(&server).await;
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|h| h == "Bearer token-for-this-server"),
        "{sent:?}"
    );
    assert!(
        !sent.iter().any(|h| h.contains(CHECKOUT_KEY)),
        "the checkout's own key is never what identifies you to a remote: {sent:?}"
    );
}

/// **The hostile-server case.** A checkout holds an `ApiKey` minted by its
/// origin (server A), and the operator is logged in to A. They then point
/// `--remote` at a server they do not control (server B) — a typo, a copied
/// command, a malicious suggestion.
///
/// B is set up to accept and answer *anything*, so if the CLI were willing
/// to send A's token the command would appear to succeed and B would walk
/// away with it. The assertions are that it does not: B receives **zero**
/// requests, A's secret appears nowhere on B's wire, and the command fails
/// closed with exit 2 before a connection is opened. Both `link` and
/// `unlink` are checked, because they take different paths to the network.
#[tokio::test]
async fn a_checkout_credential_is_never_sent_to_an_overridden_remote() {
    let origin = MockServer::start().await; // server A — the checkout's own
    let hostile = MockServer::start().await; // server B — someone else's

    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), &origin.uri()); // repo ApiKey = CHECKOUT_KEY
    login(home.path(), &origin.uri(), CHECKOUT_KEY); // and a login for A

    // B answers everything, so nothing but the CLI's own refusal can stop a
    // request from succeeding there.
    Mock::given(|_: &wiremock::Request| true)
        .respond_with(ResponseTemplate::new(200).set_body_json(feedback_list()))
        .mount(&hostile)
        .await;

    for verb in ["link", "unlink"] {
        let out = oak(
            work.path(),
            home.path(),
            &["feedback", verb, "fb-165", "--remote", &hostile.uri()],
        );
        assert_eq!(
            code(&out),
            2,
            "{verb} must fail closed against an unknown remote: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("no credential for"),
            "{verb}: {}",
            stderr(&out)
        );
    }

    let seen = hostile.received_requests().await.unwrap_or_default();
    assert!(
        seen.is_empty(),
        "the hostile server must not have been contacted at all, got {} request(s)",
        seen.len()
    );

    // Belt and braces: the secret appears nowhere on B's wire — not in a
    // header, not in a body, not in a URL.
    let wire: String = seen
        .iter()
        .map(|req| {
            format!(
                "{} {:?} {}",
                req.url,
                req.headers,
                String::from_utf8_lossy(&req.body)
            )
        })
        .collect();
    assert!(
        !wire.contains(CHECKOUT_KEY),
        "the checkout's credential reached an unrelated host"
    );

    // The origin was never called either — an overridden remote must not
    // fan out to the checkout's own server.
    assert!(origin
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// `OAK_API_KEY` is the one explicit override, and it is the caller's own
/// decision — so it applies to whatever remote they named.
#[tokio::test]
async fn an_explicit_oak_api_key_is_honored_for_the_named_remote() {
    let server = MockServer::start().await;
    let work = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    fixture_repo(work.path(), "https://origin.example");
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-1",
            Some("tester-fb"),
            None,
        )))
        .mount(&server)
        .await;

    let out = oak_env(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--remote", &server.uri()],
        &[("OAK_API_KEY", "explicit-key")],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(authorizations(&server)
        .await
        .iter()
        .all(|h| h == "Bearer explicit-key"));
}

#[tokio::test]
async fn older_status_all_rejection_is_explicit_and_never_mutates() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .and(query_param("status", "all"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error":"invalid status"})),
        )
        .mount(&server)
        .await;
    for verb in ["link", "unlink"] {
        let out = oak(
            work.path(),
            home.path(),
            &["feedback", verb, "fb-165", "--json"],
        );
        assert_eq!(code(&out), 1);
        assert!(stderr(&out).contains("status=all"));
        assert!(stderr(&out).contains("updated feedback API"));
        assert!(stdout(&out).is_empty());
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn raw_item_id_does_not_require_status_all_lookup() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-raw",
            Some("tester-fb"),
            None,
        )))
        .mount(&server)
        .await;
    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", ITEM_ID, "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn legacy_remote_url_secrets_are_not_auth_or_output_material() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    let raw = format!(
        "{}?QA_QUERY#QA_FRAGMENT",
        server
            .uri()
            .replacen("http://", "http://QA_USER:QA_PASSWORD@", 1)
    );
    fixture_repo(work.path(), &raw);
    mount_lookup(&server, 1).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-safe",
            Some("tester-fb"),
            None,
        )))
        .mount(&server)
        .await;
    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    for secret in [
        "QA_USER",
        "QA_PASSWORD",
        "QA_QUERY",
        "QA_FRAGMENT",
        "admin-token-for-this-server",
        CHECKOUT_KEY,
    ] {
        assert!(!format!("{}{}", stdout(&out), stderr(&out)).contains(secret));
    }
    assert!(authorizations(&server)
        .await
        .iter()
        .all(|header| header == "Bearer admin-token-for-this-server"));
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| !request.url.as_str().contains("QA_")));
}

#[tokio::test]
async fn server_error_cannot_echo_the_bearer_credential() {
    for token in [
        "admin-token-for-this-server".to_string(),
        "QA_LONG_SECRET".repeat(80),
    ] {
        let server = MockServer::start().await;
        let (work, home) = scene(&server);
        login(home.path(), &server.uri(), &token);
        Mock::given(method("GET"))
            .and(path_matcher("/api/feedback"))
            .respond_with(ResponseTemplate::new(500).set_body_string(format!("diagnostic {token}")))
            .mount(&server)
            .await;
        let out = oak(
            work.path(),
            home.path(),
            &["feedback", "link", "fb-165", "--json"],
        );
        assert_eq!(code(&out), 1);
        let output = format!("{}{}", stdout(&out), stderr(&out));
        assert!(!output.contains("admin-token-for-this-server"));
        assert!(!output.contains("QA_LONG_SECRET"));
    }
}

#[tokio::test]
async fn redirect_never_forwards_credentials_or_prints_sensitive_location() {
    let server = MockServer::start().await;
    let hostile = MockServer::start().await;
    let (work, home) = scene(&server);
    let location = format!("{}/?QA_REDIRECT_SECRET", hostile.uri());
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", location))
        .mount(&server)
        .await;
    let out = oak(
        work.path(),
        home.path(),
        &["feedback", "link", "fb-165", "--json"],
    );
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("do not follow redirects"));
    assert!(!format!("{}{}", stdout(&out), stderr(&out)).contains("QA_REDIRECT_SECRET"));
    assert!(hostile.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn undo_and_relink_keep_the_overridden_remote() {
    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    let (work, home) = scene(&target);
    fixture_repo(work.path(), &origin.uri());
    std::fs::write(
        home.path().join(".oak/credentials"),
        serde_json::json!([
            {"server":origin.uri(),"token":"origin-key","username":"tester"},
            {"server":target.uri(),"token":"target-key","username":"tester"}
        ])
        .to_string(),
    )
    .unwrap();
    mount_lookup(&target, 3).await;
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(link_json(
            "lnk-target",
            Some("tester-fb"),
            None,
        )))
        .expect(2)
        .mount(&target)
        .await;
    mount_links(
        &target,
        vec![link_json("lnk-target", Some("tester-fb"), None)],
    )
    .await;
    mount_delete(&target, "lnk-target", 1).await;
    let linked = oak(
        work.path(),
        home.path(),
        &[
            "feedback",
            "link",
            "fb-165",
            "--remote",
            &target.uri(),
            "--json",
        ],
    );
    assert_eq!(code(&linked), 0, "{}", stderr(&linked));
    let linked: serde_json::Value = serde_json::from_slice(&linked.stdout).unwrap();
    let undo = linked["recommended_next_commands"][0].as_str().unwrap();
    let mut undo_args: Vec<_> = undo.split_whitespace().skip(1).collect();
    undo_args.push("--json");
    let unlinked = oak(work.path(), home.path(), &undo_args);
    assert_eq!(code(&unlinked), 0, "{}", stderr(&unlinked));
    let unlinked: serde_json::Value = serde_json::from_slice(&unlinked.stdout).unwrap();
    let relink = unlinked["recommended_next_commands"][0].as_str().unwrap();
    let relink_args: Vec<_> = relink.split_whitespace().skip(1).collect();
    let restored = oak(work.path(), home.path(), &relink_args);
    assert_eq!(code(&restored), 0, "{}", stderr(&restored));
    assert!(origin.received_requests().await.unwrap().is_empty());
    assert!(authorizations(&target)
        .await
        .iter()
        .all(|header| header == "Bearer target-key"));
}

#[tokio::test]
async fn malformed_success_json_diagnostics_never_disclose_body_values() {
    for nested in [false, true] {
        for secret in ["QA_PARSE_SECRET".to_string(), "QA_PARSE_SECRET".repeat(100)] {
            let server = MockServer::start().await;
            let (work, home) = scene(&server);
            login(home.path(), &server.uri(), &secret);
            let body = if nested {
                serde_json::json!([{"id": ITEM_ID, "number": secret}])
            } else {
                serde_json::json!(secret)
            }
            .to_string()
            .replace('Q', "\\u0051");
            Mock::given(method("GET"))
                .and(path_matcher("/api/feedback"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            let out = oak(
                work.path(),
                home.path(),
                &["feedback", "link", "fb-165", "--json"],
            );
            assert_eq!(code(&out), 1);
            let diagnostic = format!("{}{}", stdout(&out), stderr(&out));
            assert!(
                !diagnostic.contains("QA_PARSE_SECRET"),
                "response value leaked (nested={nested})"
            );
            assert!(!diagnostic.contains("\\u0051"));
            assert!(stderr(&out).contains("could not parse the server's response"));
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn json_recommendations_preserve_server_values_through_a_real_shell() {
    let server = MockServer::start().await;
    let (work, home) = scene(&server);
    let hostile = "v;$(touch qa_injected)'\"`touch qa_injected`\nend";
    Mock::given(method("GET"))
        .and(path_matcher("/api/feedback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": ITEM_ID, "number":165,"ref":hostile}
        ])))
        .mount(&server)
        .await;
    let mut returned = link_json(hostile, Some(hostile), Some(hostile));
    returned["repo_owner"] = serde_json::json!(hostile);
    returned["repo_name"] = serde_json::json!(hostile);
    Mock::given(method("POST"))
        .and(path_matcher(format!("/api/feedback/{ITEM_ID}/links")))
        .respond_with(ResponseTemplate::new(200).set_body_json(returned.clone()))
        .mount(&server)
        .await;
    returned["id"] = serde_json::json!("safe-delete-id");
    mount_links(&server, vec![returned]).await;
    mount_delete(&server, "safe-delete-id", 1).await;
    for (args, expected) in [
        (
            vec!["feedback", "link", "fb-165", "--json"],
            vec!["feedback", "unlink", hostile, "--link-id", hostile],
        ),
        (
            vec![
                "feedback",
                "unlink",
                "fb-165",
                "--link-id",
                "safe-delete-id",
                "--json",
            ],
            vec![
                "feedback", "link", hostile, "--repo", "REPO", "--branch", hostile, "--commit",
                hostile,
            ],
        ),
    ] {
        let out = oak(work.path(), home.path(), &args);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let command = value["recommended_next_commands"][0].as_str().unwrap();
        let shell = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("oak() {{ printf '%s\\0' \"$@\"; }}\n{command}"))
            .env_remove("ENV")
            .env_remove("BASH_ENV")
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(shell.status.success());
        assert!(!work.path().join("qa_injected").exists());
        let actual: Vec<_> = shell
            .stdout
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        let mut expected: Vec<String> = expected
            .into_iter()
            .map(|arg| {
                if arg == "REPO" {
                    format!("{hostile}/{hostile}")
                } else {
                    arg.to_string()
                }
            })
            .collect();
        expected.extend(["--remote".to_string(), server.uri()]);
        assert_eq!(actual, expected);
    }
}
