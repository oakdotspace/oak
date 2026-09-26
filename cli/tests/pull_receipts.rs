//! `oak pull --json` (fb-319/353), `oak pull --branch-only` (fb-314), and
//! `oak desc --append` (fb-388) against a real loopback `oak serve`, plus the
//! divergence-conflict receipt against a wiremock server.
//!
//! Loopback note: `oak serve` has no hosted `main`; it answers the parent
//! ("main") head with its default branch — the first branch pushed. The
//! parent-sync conflict below is built on that behaviour: checkout A's branch
//! plays `main` for checkout B's branch.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use oak_core::{Repository, SqliteRepository};

const SERVE_TOKEN: &str = "pull-receipts-token";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn oak_with_token(home: &Path, cwd: &Path, token: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", token)
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .args(args)
        .output()
        .unwrap()
}

fn oak(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    oak_with_token(home, cwd, SERVE_TOKEN, args)
}

fn ok(output: Output, label: &str) -> Output {
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Stdout must be exactly one JSON document.
fn one_json_document(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let mut stream = serde_json::Deserializer::from_str(&stdout).into_iter::<serde_json::Value>();
    let doc = stream
        .next()
        .unwrap_or_else(|| panic!("no JSON on stdout; stderr={}", stderr(output)))
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    assert!(
        stream.next().is_none(),
        "stdout must carry exactly one JSON document: {stdout}"
    );
    doc
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

async fn start_serve(dir: &Path) -> (ChildGuard, String) {
    oak_cli::http::ensure_crypto_provider();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--token",
            SERVE_TOKEN,
        ])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = ChildGuard(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if client
            .get(format!("{base}/api/capabilities"))
            .send()
            .await
            .is_ok()
        {
            return (guard, base);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("oak serve did not become ready");
}

async fn server_description(base: &str, repo: &str, branch: &str) -> Option<String> {
    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/oak/{repo}/pull"))
        .query(&[("branch_name", branch), ("depth", "1")])
        .bearer_auth(SERVE_TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response["branch"]["description"]
        .as_str()
        .map(str::to_string)
}

fn current_branch(checkout: &Path) -> String {
    SqliteRepository::open(&checkout.join(".oak/oak.db"))
        .unwrap()
        .get_current_branch_name()
        .unwrap()
        .unwrap()
}

fn local_description(checkout: &Path, branch: &str) -> Option<String> {
    SqliteRepository::open(&checkout.join(".oak/oak.db"))
        .unwrap()
        .get_branch(branch)
        .unwrap()
        .unwrap()
        .description
}

fn branch_head(checkout: &Path, branch: &str) -> Option<String> {
    SqliteRepository::open(&checkout.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(branch)
        .unwrap()
        .map(|hash| hash.to_string())
}

struct Fixture {
    _temp: tempfile::TempDir,
    _serve: ChildGuard,
    base: String,
    home: std::path::PathBuf,
    a: std::path::PathBuf,
    a_branch: String,
}

/// Checkout A: one commit with `f.txt`, description "first", pushed to
/// `oak/<repo>` on a fresh loopback serve.
async fn published_checkout(repo: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (serve, base) = start_serve(&temp.path().join("serve-data")).await;
    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("f.txt"), "base\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit");
    ok(oak(&home, &a, &["desc", "first"]), "desc");
    ok(
        oak(
            &home,
            &a,
            &[
                "push",
                "--repo",
                &format!("oak/{repo}"),
                "-r",
                &base,
                "--json",
            ],
        ),
        "push",
    );
    let a_branch = current_branch(&a);
    Fixture {
        base,
        home,
        a_branch,
        a,
        _serve: serve,
        _temp: temp,
    }
}

/// Checkout B: a clone of `oak/<repo>` switched onto A's branch.
fn second_checkout_on(fixture: &Fixture, repo: &str, dir: &str) -> std::path::PathBuf {
    let root = fixture.a.parent().unwrap();
    ok(
        oak(
            &fixture.home,
            root,
            &["clone", &format!("oak/{repo}"), dir, "-r", &fixture.base],
        ),
        "clone",
    );
    let b = root.join(dir);
    ok(
        oak(&fixture.home, &b, &["switch", &fixture.a_branch]),
        "switch",
    );
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_json_is_one_receipt_with_narration_on_stderr() {
    let fx = published_checkout("pull-json").await;
    let head = branch_head(&fx.a, &fx.a_branch);

    let output = ok(oak(&fx.home, &fx.a, &["pull", "--json"]), "pull --json");
    let json = one_json_document(&output);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["branch"], fx.a_branch.as_str());
    assert_eq!(json["branch_after"], fx.a_branch.as_str());
    assert_eq!(json["repo"], "oak/pull-json");
    assert_eq!(json["remote"], fx.base.as_str());
    assert_eq!(json["head_before"].as_str(), head.as_deref());
    assert_eq!(json["head_after"].as_str(), head.as_deref());
    assert_eq!(json["parent"], "main");
    assert_eq!(json["commits_fetched"], 0);
    assert_eq!(json["branch_update"], "up_to_date");
    assert_eq!(json["parent_synced"], true);
    assert_eq!(json["parent_sync"], "up_to_date");
    assert_eq!(json["convergence"], "up_to_date");
    assert_eq!(json["description"], "unchanged");
    assert_eq!(json["description_refreshed"], false);
    assert_eq!(json["description_retained_local"], false);
    // Human progress stays visible, on stderr.
    let err = stderr(&output);
    assert!(err.contains("Already up to date"), "stderr: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_json_reports_fast_forward_and_refreshed_description() {
    let fx = published_checkout("pull-ff").await;
    let b = second_checkout_on(&fx, "pull-ff", "b");
    let b_head_before = branch_head(&b, &fx.a_branch);

    std::fs::write(fx.a.join("g.txt"), "more\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit a");
    ok(oak(&fx.home, &fx.a, &["desc", "second"]), "desc a");
    ok(oak(&fx.home, &fx.a, &["push", "--json"]), "push a");
    let a_head = branch_head(&fx.a, &fx.a_branch);

    let output = ok(oak(&fx.home, &b, &["pull", "--json"]), "pull --json b");
    let json = one_json_document(&output);
    assert_eq!(json["head_before"].as_str(), b_head_before.as_deref());
    assert_eq!(json["head_after"].as_str(), a_head.as_deref());
    assert_eq!(json["branch_update"], "fast_forward");
    assert_eq!(json["commits_fetched"], 1);
    assert_eq!(json["convergence"], "fast_forward");
    assert_eq!(json["parent_synced"], true);
    assert_eq!(json["description"], "refreshed");
    assert_eq!(json["description_refreshed"], true);
    assert_eq!(json["description_pending"], false);
    assert_eq!(
        local_description(&b, &fx.a_branch).as_deref(),
        Some("second")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_branch_only_skips_parent_sync_and_status_keeps_saying_so() {
    let fx = published_checkout("pull-branch-only").await;
    let b = second_checkout_on(&fx, "pull-branch-only", "b");
    let parent_before = branch_head(&b, "main");

    std::fs::write(fx.a.join("g.txt"), "more\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit a");
    ok(oak(&fx.home, &fx.a, &["desc", "second"]), "desc a");
    ok(oak(&fx.home, &fx.a, &["push", "--json"]), "push a");
    let a_head = branch_head(&fx.a, &fx.a_branch);

    let output = ok(
        oak(&fx.home, &b, &["pull", "--branch-only", "--json"]),
        "pull --branch-only --json",
    );
    let json = one_json_document(&output);
    assert_eq!(json["head_after"].as_str(), a_head.as_deref());
    assert_eq!(json["branch_update"], "fast_forward");
    assert_eq!(json["description"], "refreshed");
    assert_eq!(json["parent_synced"], false);
    assert_eq!(json["parent_sync"], "skipped");
    assert_eq!(json["convergence"], "parent_not_synced");
    assert_eq!(json["parent_head_before"], json["parent_head_after"]);
    assert_eq!(json["parent_head_after"].as_str(), parent_before.as_deref());
    assert_eq!(
        json["recommended_next_commands"],
        serde_json::json!(["oak pull --json"])
    );
    // The local parent ref was not touched: no parent refresh happened.
    assert_eq!(branch_head(&b, "main"), parent_before);
    assert!(
        !stderr(&output).contains("Updating 'main'"),
        "branch-only must not refresh the parent: {}",
        stderr(&output)
    );

    // `oak status` keeps saying the branch still needs a parent sync.
    let status = one_json_document(&ok(oak(&fx.home, &b, &["status", "--json"]), "status"));
    assert_eq!(status["parent_sync_deferred"]["parent"], "main");
    assert_eq!(status["parent_sync_deferred"]["sync_command"], "oak pull");
    let compact = one_json_document(&ok(
        oak(&fx.home, &b, &["status", "--json", "--compact"]),
        "status compact",
    ));
    assert_eq!(compact["parent_sync_deferred"]["parent"], "main");
    assert!(compact["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command == "oak pull"));

    // Human mode says so loudly.
    let human = ok(
        oak(&fx.home, &b, &["pull", "--branch-only"]),
        "pull --branch-only",
    );
    assert!(
        stderr(&human).contains("was not synced (--branch-only)"),
        "stderr: {}",
        stderr(&human)
    );

    // A full pull converges and clears the marker.
    let full = one_json_document(&ok(oak(&fx.home, &b, &["pull", "--json"]), "pull"));
    assert_eq!(full["parent_synced"], true);
    let status = one_json_document(&ok(oak(&fx.home, &b, &["status", "--json"]), "status"));
    assert!(
        status.get("parent_sync_deferred").is_none(),
        "full pull must clear the deferral: {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_json_parent_conflict_prints_receipt_then_exits_5() {
    let fx = published_checkout("pull-conflict").await;
    let root = fx.a.parent().unwrap();
    // B works on its own branch (parent: main). On loopback serve, "main"
    // resolves to A's branch, the first one pushed.
    ok(
        oak(
            &fx.home,
            root,
            &["clone", "oak/pull-conflict", "b", "-r", &fx.base],
        ),
        "clone",
    );
    let b = root.join("b");
    let b_branch = current_branch(&b);
    std::fs::write(b.join("f.txt"), "ours\n").unwrap();
    ok(oak(&fx.home, &b, &["commit"]), "commit b");

    std::fs::write(fx.a.join("f.txt"), "theirs\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit a");
    ok(oak(&fx.home, &fx.a, &["push", "--json"]), "push a");
    // Hydrate A's (= serve "main") new content locally; loopback serve has
    // no raw-content route for the parent refresh to use.
    ok(oak(&fx.home, &b, &["switch", &fx.a_branch]), "switch a");
    ok(oak(&fx.home, &b, &["pull", "--branch-only"]), "hydrate a");
    ok(oak(&fx.home, &b, &["switch", &b_branch]), "switch back");
    let b_head = branch_head(&b, &b_branch);

    let output = oak(&fx.home, &b, &["pull", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "conflict keeps the historical exit code; stderr={}",
        stderr(&output)
    );
    let json = one_json_document(&output);
    assert!(json.get("error").is_none(), "receipt, not envelope: {json}");
    assert_eq!(json["branch"], b_branch.as_str());
    assert_eq!(json["convergence"], "conflict");
    assert_eq!(json["parent_sync"], "conflict");
    assert_eq!(json["parent_synced"], false);
    assert_eq!(json["conflict_count"], 1);
    assert_eq!(json["conflict_paths"], serde_json::json!(["f.txt"]));
    assert_eq!(json["head_after"].as_str(), b_head.as_deref());
    let next = json["recommended_next_commands"].as_array().unwrap();
    assert!(next.iter().any(|c| c == "oak pull --continue"), "{json}");
    assert!(next.iter().any(|c| c == "oak pull --abort"), "{json}");
    assert!(
        stderr(&output).contains("Merge conflict"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desc_append_extends_verified_description_with_blank_line() {
    let fx = published_checkout("desc-append").await;

    let output = ok(
        oak(&fx.home, &fx.a, &["desc", "--append", "second", "--json"]),
        "desc --append",
    );
    let json = one_json_document(&output);
    assert_eq!(json["description"], "first\n\nsecond");
    assert_eq!(json["local_saved"], true);
    assert_eq!(json["remote_synced"], true);
    assert_eq!(json["append"]["base"], "remote_match");
    assert_eq!(json["append"]["previous_description"], "first");
    assert_eq!(json["append"]["appended"], "second");
    assert_eq!(
        server_description(&fx.base, "desc-append", &fx.a_branch)
            .await
            .as_deref(),
        Some("first\n\nsecond")
    );

    // --file works the same way (human mode).
    let file = fx.a.parent().unwrap().join("more.txt");
    std::fs::write(&file, "third\n").unwrap();
    ok(
        oak(
            &fx.home,
            &fx.a,
            &["desc", "--append", "--file", file.to_str().unwrap()],
        ),
        "desc --append --file",
    );
    assert_eq!(
        local_description(&fx.a, &fx.a_branch).as_deref(),
        Some("first\n\nsecond\n\nthird\n")
    );
    assert_eq!(
        server_description(&fx.base, "desc-append", &fx.a_branch)
            .await
            .as_deref(),
        Some("first\n\nsecond\n\nthird\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desc_append_refuses_stale_local_description_then_succeeds_after_refresh() {
    let fx = published_checkout("desc-stale").await;
    let b = second_checkout_on(&fx, "desc-stale", "b");
    ok(oak(&fx.home, &b, &["desc", "rewritten by b"]), "desc b");

    // A's copy ("first") is stale: refuse, change nothing anywhere.
    let output = oak(&fx.home, &fx.a, &["desc", "--append", "more", "--json"]);
    assert_eq!(output.status.code(), Some(5), "stderr={}", stderr(&output));
    let json = one_json_document(&output);
    assert_eq!(json["error"]["code"], "description_stale");
    assert_eq!(json["error"]["state"], "remote_changed");
    assert_eq!(json["error"]["retryable"], false);
    let next = json["error"]["recommended_next_commands"]
        .as_array()
        .unwrap();
    assert_eq!(next[0], "oak pull --branch-only --json");
    assert_eq!(
        local_description(&fx.a, &fx.a_branch).as_deref(),
        Some("first")
    );
    assert_eq!(
        server_description(&fx.base, "desc-stale", &fx.a_branch)
            .await
            .as_deref(),
        Some("rewritten by b")
    );

    // Human mode refuses with the same exit code and names the command.
    let human = oak(&fx.home, &fx.a, &["desc", "--append", "more"]);
    assert_eq!(human.status.code(), Some(5));
    assert!(
        stderr(&human).contains("oak pull --branch-only"),
        "stderr: {}",
        stderr(&human)
    );

    // The named refresh brings the local copy up to date; the append then
    // extends the remote text instead of clobbering it.
    ok(
        oak(&fx.home, &fx.a, &["pull", "--branch-only", "--json"]),
        "refresh",
    );
    let json = one_json_document(&ok(
        oak(&fx.home, &fx.a, &["desc", "--append", "more", "--json"]),
        "append after refresh",
    ));
    assert_eq!(json["description"], "rewritten by b\n\nmore");
    assert_eq!(
        server_description(&fx.base, "desc-stale", &fx.a_branch)
            .await
            .as_deref(),
        Some("rewritten by b\n\nmore")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desc_append_refuses_pending_local_edit_that_differs_from_remote() {
    let fx = published_checkout("desc-pending").await;
    // A local edit whose sync was rejected stays pending.
    let rejected = oak_with_token(&fx.home, &fx.a, "wrong-token", &["desc", "local only"]);
    assert!(rejected.status.success(), "local save still succeeds");
    assert!(SqliteRepository::open(&fx.a.join(".oak/oak.db"))
        .unwrap()
        .branch_description_pending(&fx.a_branch)
        .unwrap());

    let output = oak(&fx.home, &fx.a, &["desc", "--append", "more", "--json"]);
    assert_eq!(output.status.code(), Some(5), "stderr={}", stderr(&output));
    let json = one_json_document(&output);
    assert_eq!(json["error"]["code"], "description_stale");
    assert_eq!(json["error"]["state"], "pending_local_edit");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unpublished edits"),
        "{json}"
    );
    assert_eq!(
        local_description(&fx.a, &fx.a_branch).as_deref(),
        Some("local only")
    );
    assert_eq!(
        server_description(&fx.base, "desc-pending", &fx.a_branch)
            .await
            .as_deref(),
        Some("first")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desc_append_refuses_when_remote_cannot_be_read() {
    let fx = published_checkout("desc-unverified").await;
    let output = oak_with_token(
        &fx.home,
        &fx.a,
        "wrong-token",
        &["desc", "--append", "more", "--json"],
    );
    // A transport-class failure: exit 6, retryable, typed state.
    assert_eq!(output.status.code(), Some(6), "stderr={}", stderr(&output));
    let json = one_json_document(&output);
    assert_eq!(json["error"]["code"], "description_stale");
    assert_eq!(json["error"]["state"], "remote_unverified");
    assert_eq!(json["error"]["retryable"], true);
    assert_eq!(json["error"]["branch"], fx.a_branch.as_str());
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not be read"),
        "{json}"
    );
    assert_eq!(
        local_description(&fx.a, &fx.a_branch).as_deref(),
        Some("first")
    );
}

#[test]
fn desc_append_without_remote_appends_locally() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    ok(oak(&home, &checkout, &["init", "."]), "init");
    ok(oak(&home, &checkout, &["desc", "one"]), "desc");
    let json = one_json_document(&ok(
        oak(&home, &checkout, &["desc", "--append", "two", "--json"]),
        "append",
    ));
    assert_eq!(json["description"], "one\n\ntwo");
    assert_eq!(json["remote_synced"], "not_linked");
    assert_eq!(json["append"]["base"], "not_linked");

    // An empty append is a usage error, not a silent no-op.
    let empty = oak(&home, &checkout, &["desc", "--append", "  ", "--json"]);
    assert_eq!(empty.status.code(), Some(2));
}

#[test]
fn pull_json_and_branch_only_reject_continue_and_abort() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        ["pull", "--json", "--continue"],
        ["pull", "--branch-only", "--abort"],
    ] {
        let output = oak(temp.path(), temp.path(), &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

/// Commits the server holds on `branch` after `since` (exclusive), in the
/// server's order.
async fn server_commits_since(base: &str, repo: &str, branch: &str, since: &str) -> Vec<String> {
    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/oak/{repo}/pull"))
        .query(&[("branch_name", branch), ("since", since)])
        .bearer_auth(SERVE_TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["hash"].as_str().unwrap().to_string())
        .collect()
}

async fn server_branch_head(base: &str, repo: &str, branch: &str) -> Option<String> {
    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/oak/{repo}/branches/{branch}"))
        .bearer_auth(SERVE_TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response["head"].as_str().map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_plan_is_read_only_and_matches_what_push_sends() {
    let fx = published_checkout("push-plan").await;
    let pushed = server_branch_head(&fx.base, "push-plan", &fx.a_branch)
        .await
        .unwrap();

    // Nothing outgoing yet.
    let json = one_json_document(&ok(
        oak(&fx.home, &fx.a, &["push", "--plan", "--json"]),
        "plan (up to date)",
    ));
    assert_eq!(json["up_to_date"], true);
    assert_eq!(json["commit_count"], 0);
    assert_eq!(json["plan_complete"], true);

    // Two local commits: a new file, then an edit.
    std::fs::write(fx.a.join("g.txt"), "new file\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit 1");
    std::fs::write(fx.a.join("f.txt"), "edited\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit 2");
    let local_head = branch_head(&fx.a, &fx.a_branch).unwrap();

    let output = ok(oak(&fx.home, &fx.a, &["push", "--plan", "--json"]), "plan");
    let plan = one_json_document(&output);
    assert_eq!(plan["plan"], true);
    assert_eq!(plan["mutated"], false);
    assert_eq!(plan["plan_complete"], true, "{plan}");
    assert_eq!(plan["repo"], "oak/push-plan");
    assert_eq!(plan["branch"], fx.a_branch.as_str());
    assert_eq!(plan["local_head"], local_head.as_str());
    assert_eq!(plan["remote_head"], pushed.as_str());
    assert_eq!(plan["up_to_date"], false);
    assert_eq!(plan["commit_count"], 2);
    let commits = plan["commits"].as_array().unwrap();
    // Parent-before-child, closing over the server boundary.
    assert_eq!(commits[0]["parent"], pushed.as_str());
    assert_eq!(commits[1]["parent"], commits[0]["hash"]);
    assert_eq!(commits[1]["hash"], local_head.as_str());
    // The outgoing trees reference three blobs: the unchanged `f.txt` base
    // (already on the server) plus two new ones.
    assert_eq!(plan["blob_count"], 3);
    assert_eq!(plan["blobs_server_has"], 1);
    assert_eq!(plan["blobs_missing_on_server_count"], 2);
    assert_eq!(plan["blobs_missing_on_server"].as_array().unwrap().len(), 2);

    // Read-only: the server branch did not move.
    assert_eq!(
        server_branch_head(&fx.base, "push-plan", &fx.a_branch)
            .await
            .as_deref(),
        Some(pushed.as_str())
    );

    // The real push sends exactly the planned commits.
    let push = one_json_document(&ok(oak(&fx.home, &fx.a, &["push", "--json"]), "push"));
    assert_eq!(push["pushed_head"], local_head.as_str());
    let planned: Vec<String> = commits
        .iter()
        .map(|commit| commit["hash"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        server_commits_since(&fx.base, "push-plan", &fx.a_branch, &pushed).await,
        planned
    );

    // Unchanged content re-used by a new commit is reported as present.
    std::fs::write(fx.a.join("h.txt"), "new file\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit 3");
    let plan = one_json_document(&ok(
        oak(&fx.home, &fx.a, &["push", "--plan", "--json"]),
        "plan 3",
    ));
    assert_eq!(plan["commit_count"], 1);
    assert_eq!(plan["blobs_server_has"], plan["blob_count"]);
    assert_eq!(plan["blobs_missing_on_server_count"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_plan_reports_divergence_instead_of_guessing() {
    let fx = published_checkout("push-plan-diverged").await;
    let b = second_checkout_on(&fx, "push-plan-diverged", "b");
    std::fs::write(b.join("theirs.txt"), "theirs\n").unwrap();
    ok(oak(&fx.home, &b, &["commit"]), "commit b");
    ok(oak(&fx.home, &b, &["push", "--json"]), "push b");
    let server_head = server_branch_head(&fx.base, "push-plan-diverged", &fx.a_branch).await;

    std::fs::write(fx.a.join("ours.txt"), "ours\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "commit a");
    let local_before = branch_head(&fx.a, &fx.a_branch);
    let plan = one_json_document(&ok(
        oak(&fx.home, &fx.a, &["push", "--plan", "--json"]),
        "plan",
    ));
    assert_eq!(plan["plan_complete"], false);
    assert_eq!(plan["incomplete"][0]["reason"], "diverged");
    assert!(plan["commit_count"].is_null());
    // Planning did not re-parent locally or move the server.
    assert_eq!(branch_head(&fx.a, &fx.a_branch), local_before);
    assert_eq!(
        server_branch_head(&fx.base, "push-plan-diverged", &fx.a_branch).await,
        server_head
    );
}

#[test]
fn push_plan_requires_json_and_a_linked_repository() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    let usage = oak(&home, &checkout, &["push", "--plan"]);
    assert_eq!(usage.status.code(), Some(2));

    ok(oak(&home, &checkout, &["init", "."]), "init");
    std::fs::write(checkout.join("f.txt"), "x\n").unwrap();
    ok(oak(&home, &checkout, &["commit"]), "commit");
    let output = oak(
        &home,
        &checkout,
        &["push", "--plan", "--json", "-r", "http://127.0.0.1:9"],
    );
    assert_eq!(output.status.code(), Some(2), "stderr={}", stderr(&output));
    let json = one_json_document(&output);
    assert_eq!(json["error"]["code"], "invalid_argument");
    // The plan never persisted the remote it was given.
    let repo = SqliteRepository::open(&checkout.join(".oak/oak.db")).unwrap();
    assert!(repo
        .get_metadata(oak_core::MetadataKey::RemoteUrl)
        .unwrap()
        .is_none());
}

/// QA-W3a M1: a pull that moves the branch head to a commit that does not
/// contain the previous head (here: an unpushed local commit is dropped by
/// the pre-existing pull/sync behaviour) must never be reported as a benign
/// re-parent. The receipt says `replaced`, flags the unknown, warns, and
/// names inspection commands for the old head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_json_reports_non_descendant_head_move_as_replaced() {
    let fx = published_checkout("pull-replaced").await;
    std::fs::write(fx.a.join("local.txt"), "unpushed\n").unwrap();
    ok(oak(&fx.home, &fx.a, &["commit"]), "local commit");
    let local_head = branch_head(&fx.a, &fx.a_branch).unwrap();

    let output = oak(&fx.home, &fx.a, &["pull", "--json"]);
    let json = one_json_document(&output);
    assert_eq!(json["head_before"], local_head.as_str());
    if json["head_after"] == json["head_before"] {
        // The underlying discard (owned elsewhere) was fixed: nothing moved,
        // so the receipt has nothing to disclose.
        assert_eq!(json["branch_update"], "up_to_date");
        return;
    }
    assert_eq!(json["branch_update"], "replaced", "{json}");
    assert_eq!(json["convergence"], "replaced", "{json}");
    assert_ne!(json["convergence"], "reparented");
    assert!(json["unknowns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field == "local_commits_preserved"));
    let next = json["recommended_next_commands"].as_array().unwrap();
    assert_eq!(next[0], format!("oak diff {local_head} --stat").as_str());
    assert!(
        stderr(&output).contains("does not contain it"),
        "stderr: {}",
        stderr(&output)
    );
}

/// QA-W3a M2: the deferral is per branch. A full pull on another branch
/// neither clears nor overwrites it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_sync_deferral_is_per_branch() {
    let fx = published_checkout("defer-per-branch").await;
    let root = fx.a.parent().unwrap();
    ok(
        oak(
            &fx.home,
            root,
            &["clone", "oak/defer-per-branch", "b", "-r", &fx.base],
        ),
        "clone",
    );
    let b = root.join("b");
    let b_branch = current_branch(&b);

    ok(
        oak(&fx.home, &b, &["pull", "--branch-only"]),
        "branch-only on B",
    );
    // Another branch: its own branch-only deferral, then a full pull.
    ok(oak(&fx.home, &b, &["switch", &fx.a_branch]), "switch A");
    ok(
        oak(&fx.home, &b, &["pull", "--branch-only"]),
        "branch-only on A",
    );
    ok(oak(&fx.home, &b, &["pull"]), "full pull on A");
    let status = one_json_document(&ok(oak(&fx.home, &b, &["status", "--json"]), "status A"));
    assert!(status.get("parent_sync_deferred").is_none(), "{status}");

    ok(oak(&fx.home, &b, &["switch", &b_branch]), "switch B");
    let status = one_json_document(&ok(oak(&fx.home, &b, &["status", "--json"]), "status B"));
    assert_eq!(status["parent_sync_deferred"]["parent"], "main", "{status}");
    // `oak agent state` surfaces it too.
    let state = one_json_document(&ok(
        oak(&fx.home, &b, &["agent", "state", "--json", "--compact"]),
        "agent state",
    ));
    assert_eq!(state["parent_sync_deferred"]["parent"], "main", "{state}");
    assert!(state["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command == "oak pull"));

    // B's own full pull clears B's entry.
    ok(oak(&fx.home, &b, &["pull"]), "full pull on B");
    let status = one_json_document(&ok(oak(&fx.home, &b, &["status", "--json"]), "status B2"));
    assert!(status.get("parent_sync_deferred").is_none(), "{status}");
}

/// QA-W3a L2: the previous description is preserved byte for byte.
#[test]
fn desc_append_preserves_previous_bytes_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("c");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    ok(oak(&home, &checkout, &["init", "."]), "init");
    let previous = "line one\r\nünïcode\ttrailing\t\n\n";
    let file = temp.path().join("prev.txt");
    std::fs::write(&file, previous).unwrap();
    ok(
        oak(
            &home,
            &checkout,
            &["desc", "--file", file.to_str().unwrap()],
        ),
        "desc",
    );
    let json = one_json_document(&ok(
        oak(&home, &checkout, &["desc", "--append", "added", "--json"]),
        "append",
    ));
    assert_eq!(json["append"]["previous_description"], previous);
    assert_eq!(
        json["description"].as_str().unwrap(),
        format!("{previous}\n\nadded")
    );
}
