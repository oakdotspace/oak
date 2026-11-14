//! Adversarial close-many tests from independent QA (W4b), adopted as
//! regressions. fb-413: `oak close --remote A@HEAD B@HEAD ... --reason R --json` closes
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

struct ListBranches(Arc<Mutex<RemoteState>>, Arc<Mutex<u32>>, Option<u32>);

impl Respond for ListBranches {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        assert_eq!(
            req.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string()),
            Some("Bearer test-token".to_string())
        );
        let mut n = self.1.lock().unwrap();
        *n += 1;
        if Some(*n) == self.2 {
            return ResponseTemplate::new(503);
        }
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
        if name == "conflict" {
            return ResponseTemplate::new(409);
        }
        if name == "redirect" {
            return ResponseTemplate::new(307).insert_header("location", "http://127.0.0.1:1/x");
        }
        if name == "softfail" {
            return ResponseTemplate::new(200)
                .set_body_json(json!({"success": false, "message": "nope"}));
        }
        if name == "badbody" {
            return ResponseTemplate::new(200).set_body_string("not json");
        }
        if name == "slowraced" {
            // head moves while the close request is in flight, before it lands
            for branch in &mut state.branches {
                if branch["name"] == name {
                    branch["head"] = json!(h('8'));
                }
            }
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

async fn fixture(
    branches: Vec<Value>,
    fail_list_call: Option<u32>,
) -> (tempfile::TempDir, MockServer, Arc<Mutex<RemoteState>>) {
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
        .respond_with(ListBranches(
            state.clone(),
            Arc::new(Mutex::new(0)),
            fail_list_call,
        ))
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
async fn qa_lone_raced_close_is_not_success() {
    let (temp, _s, _state) = fixture(
        vec![
            branch("main", &h('0'), "open"),
            branch("raced", &h('6'), "open"),
        ],
        None,
    )
    .await;
    let a = format!("raced@{}", h('6'));
    let out = oak(temp.path(), &["close", "--remote", &a, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(5), "{v}");
    assert_eq!(v["all_closed"], false);
    let r = row(&v, "raced");
    assert_eq!(r["outcome"], "closed_after_head_moved");
    assert_eq!(r["fence_verified"], false);
    assert_eq!(r["final_observed_head"], h('9'));
}

#[tokio::test(flavor = "current_thread")]
async fn qa_head_moved_before_close_lands_is_not_success() {
    let (temp, _s, _state) = fixture(
        vec![
            branch("main", &h('0'), "open"),
            branch("slowraced", &h('6'), "open"),
        ],
        None,
    )
    .await;
    let a = format!("slowraced@{}", h('6'));
    let out = oak(temp.path(), &["close", "--remote", &a, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(5), "{v}");
    assert_eq!(v["all_closed"], false);
    assert_eq!(row(&v, "slowraced")["outcome"], "closed_after_head_moved");
}

#[tokio::test(flavor = "current_thread")]
async fn qa_final_listing_failure_leaves_fence_unchecked() {
    // list call 1 = per-target read; call 2 = final listing -> 503
    let (temp, _s, _state) = fixture(
        vec![
            branch("main", &h('0'), "open"),
            branch("raced", &h('6'), "open"),
        ],
        Some(2),
    )
    .await;
    let a = format!("raced@{}", h('6'));
    let out = oak(temp.path(), &["close", "--remote", &a, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(6), "{v}");
    assert_eq!(v["all_closed"], false);
    assert!(v["open_branches"].is_null());
    assert!(v["open_branches_unavailable_reason"].is_string());
    let r = row(&v, "raced");
    assert_eq!(r["outcome"], "closed");
    assert_eq!(r["fence_verified"], "unchecked");
}

#[tokio::test(flavor = "current_thread")]
async fn qa_first_listing_failure_is_isolated_to_its_row() {
    let (temp, _s, state) = fixture(
        vec![
            branch("main", &h('0'), "open"),
            branch("a", &h('1'), "open"),
            branch("b", &h('2'), "open"),
        ],
        Some(1),
    )
    .await;
    let a = format!("a@{}", h('1'));
    let b = format!("b@{}", h('2'));
    let out = oak(temp.path(), &["close", "--remote", &a, &b, "--json"]);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(6), "{v}");
    assert_eq!(row(&v, "a")["outcome"], "read_failed");
    assert_eq!(row(&v, "a")["close_request_sent"], false);
    assert_eq!(row(&v, "b")["outcome"], "closed");
    assert_eq!(state.lock().unwrap().pushes, vec!["b"]);
}

#[tokio::test(flavor = "current_thread")]
async fn qa_conclusive_and_ambiguous_classes() {
    let names = [
        "conflict",
        "redirect",
        "softfail",
        "badbody",
        "ambiguous",
        "ok",
    ];
    let mut bs = vec![branch("main", &h('0'), "open")];
    for n in names {
        bs.push(branch(n, &h('1'), "open"));
    }
    let (temp, _s, state) = fixture(bs, None).await;
    let dir = temp.path();
    let args: Vec<String> = names.iter().map(|n| format!("{n}@{}", h('1'))).collect();
    let mut argv = vec!["close", "--remote", "--reason", "stale"];
    argv.extend(args.iter().map(String::as_str));
    argv.push("--json");
    let out = oak(dir, &argv);
    let v = one_json(&out);
    assert_eq!(out.status.code(), Some(6), "{v}");
    for (n, expected) in [
        ("conflict", "rejected"),
        ("softfail", "rejected"),
        ("redirect", "unknown_outcome"),
        ("badbody", "unknown_outcome"),
        ("ambiguous", "unknown_outcome"),
        ("ok", "closed"),
    ] {
        assert_eq!(row(&v, n)["outcome"], expected, "{n}: {v}");
        assert_eq!(row(&v, n)["close_request_sent"], true);
        let local = SqliteRepository::open(&dir.join(".oak/oak.db"))
            .unwrap()
            .get_branch(n)
            .unwrap()
            .unwrap();
        if n == "ok" {
            assert_eq!(local.status, oak_core::BranchStatus::Closed);
        } else {
            assert_eq!(local.status, oak_core::BranchStatus::Open, "{n}");
            assert!(local.close_reason.is_none(), "{n}");
            assert!(row(&v, n).get("local_rollback_error").is_none());
        }
    }
    // Exactly one request per branch: nothing is retried.
    assert_eq!(state.lock().unwrap().pushes, names.to_vec());
}

#[tokio::test(flavor = "current_thread")]
async fn qa_argument_validation() {
    let (temp, _s, state) = fixture(
        vec![
            branch("main", &h('0'), "open"),
            branch("a", &h('1'), "open"),
        ],
        None,
    )
    .await;
    let dir = temp.path();
    let many: Vec<String> = (0..101).map(|i| format!("b{i}@{}", h('1'))).collect();
    let mut argv = vec!["close", "--remote", "--json"];
    argv.extend(many.iter().map(String::as_str));
    assert_eq!(oak(dir, &argv).status.code(), Some(2));
    let m = format!("main@{}", h('0'));
    let upper = format!("a@{}", h('A'));
    for args in [
        vec!["close", "--remote", "--json", m.as_str()],
        vec!["close", "--remote", "--json", "a@1111111"],
        vec!["close", "--remote", "--json", upper.as_str()],
    ] {
        let out = oak(dir, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let v = one_json(&out);
        assert_eq!(v["error"]["code"], "invalid_argument", "{args:?}: {v}");
    }
    assert!(state.lock().unwrap().pushes.is_empty());
}
