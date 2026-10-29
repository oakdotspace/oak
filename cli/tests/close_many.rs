//! fb-413: `oak close --remote A@HEAD B@HEAD ... --reason R --json` closes
//! several remote branches checkout-free, fenced per branch by its exact head,
//! with typed partial-failure rows and the final open-branch list.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn h(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("HOME", dir.join(".home"))
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn one_json(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<Value>();
    let doc = stream
        .next()
        .unwrap_or_else(|| {
            panic!(
                "no JSON on stdout; stderr={}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .unwrap();
    assert!(stream.next().is_none(), "exactly one JSON document: {text}");
    doc
}

#[derive(Default)]
struct RemoteState {
    branches: Vec<Value>,
    pushes: Vec<String>,
}

struct ListBranches(Arc<Mutex<RemoteState>>);

impl Respond for ListBranches {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let state = self.0.lock().unwrap();
        ResponseTemplate::new(200).set_body_json(json!({ "branches": state.branches }))
    }
}

/// Metadata-only push: records the branch, applies the close, and simulates
/// (a) an ambiguous 500 for `ambiguous` and (b) a push that races the fence
/// for `raced` by moving its head while the close lands.
struct Push(Arc<Mutex<RemoteState>>);

impl Respond for Push {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body["commits"].as_array().unwrap().is_empty());
        let name = body["branch"]["name"].as_str().unwrap().to_string();
        let mut state = self.0.lock().unwrap();
        state.pushes.push(name.clone());
        if name == "ambiguous" {
            return ResponseTemplate::new(500);
        }
        for branch in &mut state.branches {
            if branch["name"] == name {
                branch["status"] = body["branch"]["status"].clone();
                branch["close_reason"] = body["branch"]["close_reason"].clone();
                if name == "raced" {
                    branch["head"] = json!(h('9'));
                }
            }
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "new_head": null,
            "message": "ok"
        }))
    }
}

fn branch(name: &str, head: &str, status: &str) -> Value {
    json!({
        "name": name,
        "head": head,
        "description": format!("{name} description"),
        "parent_branch": "main",
        "status": status,
        "created_at": "2026-01-02T00:00:00Z"
    })
}

async fn fixture(branches: Vec<Value>) -> (tempfile::TempDir, MockServer, Arc<Mutex<RemoteState>>) {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    std::fs::create_dir_all(dir.join(".home")).unwrap();
    assert!(oak(dir, &["init", "."]).status.success());
    std::fs::write(dir.join("tracked.txt"), "base\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let state = Arc::new(Mutex::new(RemoteState {
        branches,
        pushes: Vec::new(),
    }));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(ListBranches(state.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/push"))
        .respond_with(Push(state.clone()))
        .mount(&server)
        .await;
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
    repo.set_metadata(MetadataKey::ApiKey, "test-token")
        .unwrap();
    (temp, server, state)
}

fn row<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == name)
        .unwrap_or_else(|| panic!("no row for {name}: {value}"))
}

#[tokio::test(flavor = "current_thread")]
async fn close_many_fences_each_branch_and_reports_typed_partial_failures() {
    let (temp, _server, state) = fixture(vec![
        branch("main", &h('0'), "open"),
        branch("keep-open", &h('0'), "open"),
        branch("ok", &h('1'), "open"),
        branch("moved", &h('2'), "open"),
        branch("done", &h('4'), "closed"),
        branch("ambiguous", &h('5'), "open"),
        branch("raced", &h('6'), "open"),
    ])
    .await;
    let dir = temp.path();
    let current_before = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_current_branch_name()
        .unwrap();
    let args = [
        format!("ok@{}", h('1')),
        format!("moved@{}", h('3')), // pinned to a stale head
        format!("gone@{}", h('1')),
        format!("done@{}", h('4')),
        format!("ambiguous@{}", h('5')),
        format!("raced@{}", h('6')),
    ];
    let mut argv = vec!["close", "--remote"];
    argv.extend(args.iter().map(String::as_str));
    argv.extend(["--reason", "superseded", "--json"]);
    let out = oak(dir, &argv);
    let value = one_json(&out);
    // An unconfirmed close is present, so the invocation is transport-class.
    assert_eq!(out.status.code(), Some(6), "{value}");

    assert_eq!(value["operation"], "close_many");
    assert_eq!(value["all_closed"], false);
    assert_eq!(value["fence"]["kind"], "read_then_close");
    assert_eq!(value["fence"]["server_precondition"], false);
    assert_eq!(value["close_reason"], "superseded");

    assert_eq!(row(&value, "ok")["outcome"], "closed");
    assert_eq!(row(&value, "ok")["close_request_sent"], true);
    assert_eq!(row(&value, "ok")["final_observed_status"], "closed");
    assert_eq!(row(&value, "ok")["fence_verified"], true);

    let moved = row(&value, "moved");
    assert_eq!(moved["outcome"], "refused_head_moved");
    assert_eq!(moved["observed_head"], h('2'));
    assert_eq!(moved["close_request_sent"], false);
    assert_eq!(moved["final_observed_status"], "open");

    assert_eq!(row(&value, "gone")["outcome"], "not_found");
    assert_eq!(row(&value, "done")["outcome"], "already_closed");
    assert_eq!(row(&value, "done")["close_request_sent"], false);

    let ambiguous = row(&value, "ambiguous");
    assert_eq!(ambiguous["outcome"], "unknown_outcome");
    assert_eq!(ambiguous["close_request_sent"], true);
    assert!(ambiguous["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "oak branch list --remote --json"));

    let raced = row(&value, "raced");
    assert_eq!(raced["outcome"], "closed_after_head_moved");
    assert_eq!(raced["head_moved_after_fence"], true);
    assert_eq!(raced["fence_verified"], false);
    assert_eq!(raced["final_observed_head"], h('9'));

    // Only branches whose pin matched were ever sent a close, each exactly once
    // (the ambiguous one is not retried).
    let pushes = state.lock().unwrap().pushes.clone();
    assert_eq!(pushes, vec!["ok", "ambiguous", "raced"]);

    let open: Vec<&str> = value["open_branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(open, vec!["ambiguous", "keep-open", "main", "moved"]);
    assert_eq!(value["counts"]["closed"], 1);
    assert_eq!(value["counts"]["closed_after_head_moved"], 1);
    assert_eq!(value["counts"]["refused_head_moved"], 1);
    assert_eq!(value["counts"]["unknown_outcome"], 1);

    // Checkout-free: the current branch did not change.
    let current_after = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_current_branch_name()
        .unwrap();
    assert_eq!(current_before, current_after);

    // The local mirror records only confirmed closes.
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    let status = |name: &str| repo.get_branch(name).unwrap().map(|b| b.status);
    assert_eq!(status("ok"), Some(oak_core::BranchStatus::Closed));
    assert_eq!(status("ambiguous"), Some(oak_core::BranchStatus::Open));
    // The rollback also clears the reason written for the attempt.
    assert!(repo
        .get_branch("ambiguous")
        .unwrap()
        .unwrap()
        .close_reason
        .is_none());
    assert_eq!(status("moved"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn close_many_exits_zero_when_every_branch_ends_closed() {
    let (temp, _server, state) = fixture(vec![
        branch("main", &h('0'), "open"),
        branch("a", &h('1'), "open"),
        branch("b", &h('2'), "closed"),
    ])
    .await;
    let a = format!("a@{}", h('1'));
    let b = format!("b@{}", h('2'));
    let out = oak(temp.path(), &["close", "--remote", &a, &b, "--json"]);
    let value = one_json(&out);
    assert_eq!(out.status.code(), Some(0), "{value}");
    assert_eq!(value["all_closed"], true);
    assert_eq!(state.lock().unwrap().pushes, vec!["a"]);
}

#[tokio::test(flavor = "current_thread")]
async fn close_many_refuses_moved_only_run_with_conflict_exit_and_no_mutation() {
    let (temp, _server, state) = fixture(vec![
        branch("main", &h('0'), "open"),
        branch("a", &h('1'), "open"),
    ])
    .await;
    let a = format!("a@{}", h('7'));
    let out = oak(temp.path(), &["close", "--remote", &a, "--json"]);
    let value = one_json(&out);
    assert_eq!(out.status.code(), Some(5), "{value}");
    assert_eq!(row(&value, "a")["outcome"], "refused_head_moved");
    assert!(state.lock().unwrap().pushes.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn close_many_argument_errors_are_refused_before_any_request() {
    let (temp, _server, state) = fixture(vec![branch("a", &h('1'), "open")]).await;
    let dir = temp.path();
    let fenced = format!("a@{}", h('1'));
    for args in [
        // several branches require --remote --json
        vec!["close", "a", "b", "--json"],
        vec!["close", "--remote", fenced.as_str()],
        // mixing fenced and unfenced targets defeats the fence
        vec!["close", "--remote", fenced.as_str(), "b", "--json"],
        vec!["close", "--remote", "a@abc", "b", "--json"],
    ] {
        let out = oak(dir, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {:?}", out);
    }
    assert!(state.lock().unwrap().pushes.is_empty());
}

fn head_of(dir: &Path, branch: &str) -> String {
    SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head(branch)
        .unwrap()
        .unwrap()
        .to_string()
}

/// Local branch `name` with two commits; returns (c1, c2).
fn two_local_commits(dir: &Path, name: &str) -> (String, String) {
    assert!(oak(dir, &["switch", "-c", name]).status.success());
    std::fs::write(dir.join(format!("{name}-1.txt")), "one\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    let c1 = head_of(dir, name);
    std::fs::write(dir.join(format!("{name}-2.txt")), "two\n").unwrap();
    assert!(oak(dir, &["commit"]).status.success());
    (c1, head_of(dir, name))
}

fn status_change_count(dir: &Path) -> usize {
    let out = oak(dir, &["status", "--json"]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["changes"].as_array().unwrap().len()
}

/// QA W4b (pre-existing High): a remote close must never move a local head
/// that is ahead of the remote; that would orphan local-only commits.
#[tokio::test(flavor = "current_thread")]
async fn remote_close_keeps_a_local_head_that_is_ahead_of_the_remote() {
    let (temp, _server, _state) = fixture(vec![branch("main", &h('0'), "open")]).await;
    let dir = temp.path();
    let (c1, c2) = two_local_commits(dir, "feat");
    let (d1, d2) = two_local_commits(dir, "solo");
    {
        let mut state = _state.lock().unwrap();
        state.branches.push(branch("feat", &c1, "open"));
        state.branches.push(branch("solo", &d1, "open"));
    }
    assert_eq!(status_change_count(dir), 0);

    // close-many
    let feat = format!("feat@{c1}");
    let out = oak(dir, &["close", "--remote", &feat, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(0), "{v}");
    let r = row(&v, "feat");
    assert_eq!(r["outcome"], "closed");
    assert_eq!(r["local_head"]["relation"], "local_ahead");
    assert_eq!(r["local_ahead"], 1);
    assert_eq!(r["local_only_commits"], true);
    assert_eq!(head_of(dir, "feat"), c2, "local head must not move back");

    // single close
    let out = oak(dir, &["close", "--remote", "solo", "--json"]);
    let v = one_json(&out);
    assert!(out.status.success(), "{v}");
    assert_eq!(v["local_head"]["relation"], "local_ahead");
    assert_eq!(v["local_ahead"], 1);
    assert_eq!(v["local_only_commits"], true);
    assert_eq!(head_of(dir, "solo"), d2, "local head must not move back");

    // Local-only commits are still committed: nothing reads as uncommitted.
    assert_eq!(status_change_count(dir), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn remote_close_keeps_a_diverged_local_head() {
    let (temp, _server, state) = fixture(vec![branch("main", &h('0'), "open")]).await;
    let dir = temp.path();
    let (_, e2) = two_local_commits(dir, "div");
    let (_, f2) = two_local_commits(dir, "div2");
    // A real local commit with no shared ancestry (a root commit on another
    // branch), standing in for a remote head that diverged.
    let other = {
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        let tree = oak_core::Tree::new(Vec::new()).unwrap();
        repo.store_tree(&tree).unwrap();
        let commit = oak_core::Commit::new(
            "other".to_string(),
            None,
            None,
            tree.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        commit.hash.to_string()
    };
    {
        // The remote heads are real local commits unrelated to the branches.
        let mut state = state.lock().unwrap();
        state.branches.push(branch("div", &other, "open"));
        state.branches.push(branch("div2", &other, "open"));
    }
    let div = format!("div@{other}");
    let out = oak(dir, &["close", "--remote", &div, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(0), "{v}");
    assert_eq!(row(&v, "div")["local_head"]["relation"], "diverged");
    assert_eq!(row(&v, "div")["local_only_commits"], true);
    assert_eq!(head_of(dir, "div"), e2);

    let out = oak(dir, &["close", "--remote", "div2", "--json"]);
    let v = one_json(&out);
    assert!(out.status.success(), "{v}");
    assert_eq!(v["local_head"]["relation"], "diverged");
    assert_eq!(head_of(dir, "div2"), f2);
    assert_eq!(status_change_count(dir), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn remote_close_still_fast_forwards_a_local_head_that_is_behind() {
    let (temp, _server, state) = fixture(vec![branch("main", &h('0'), "open")]).await;
    let dir = temp.path();
    let (c1, c2) = two_local_commits(dir, "behind");
    // Local is behind the remote head: rewind the local pointer to c1.
    SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .set_branch_head("behind", &oak_core::Hash::from_hex(&c1).unwrap())
        .unwrap();
    state
        .lock()
        .unwrap()
        .branches
        .push(branch("behind", &c2, "open"));
    let pin = format!("behind@{c2}");
    let out = oak(dir, &["close", "--remote", &pin, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(0), "{v}");
    assert_eq!(
        row(&v, "behind")["local_head"]["relation"],
        "fast_forwarded"
    );
    assert!(row(&v, "behind").get("local_only_commits").is_none());
    assert_eq!(head_of(dir, "behind"), c2);
}

/// `name@<hex>` is ambiguous with a branch literally named that way.
#[tokio::test(flavor = "current_thread")]
async fn short_hex_suffix_is_a_branch_name_when_that_branch_exists_locally() {
    let (temp, _server, state) = fixture(vec![branch("main", &h('0'), "open")]).await;
    let dir = temp.path();
    let (_, head) = two_local_commits(dir, "rel@abcd1234");
    state
        .lock()
        .unwrap()
        .branches
        .push(branch("rel@abcd1234", &head, "open"));
    let out = oak(dir, &["close", "--remote", "rel@abcd1234", "--json"]);
    let v = one_json(&out);
    assert!(out.status.success(), "{v}");
    assert_eq!(v["branch"], "rel@abcd1234");
    assert_eq!(v["status"], "closed");
    // Without such a branch the same shape is a malformed pin: typed refusal.
    let out = oak(dir, &["close", "--remote", "zzz@abcd1234", "--json"]);
    assert_eq!(out.status.code(), Some(2));
}
