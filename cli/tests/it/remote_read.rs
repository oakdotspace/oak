//! `oak ls --remote` / `oak grep --remote`: peek at the remote without
//! widening the checkout. The branch head is pinned once, listings are
//! sorted and bounded, search results can be narrowed to path prefixes, and
//! nothing local changes.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("HOME", dir.parent().unwrap())
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn json_stdout(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

const HEAD: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// A checkout at `<tmp>/repo` (with `<tmp>` as HOME) linked to `server`.
struct Checkout {
    _temp: tempfile::TempDir,
    dir: std::path::PathBuf,
}

impl Checkout {
    fn path(&self) -> &Path {
        &self.dir
    }
}

async fn linked_checkout(server: &MockServer) -> Checkout {
    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path().join("repo");
    std::fs::create_dir(&dir).unwrap();
    let checkout_dir = dir.clone();
    let uri = server.uri();
    tokio::task::spawn_blocking(move || {
        assert!(oak(&dir, &["init", "."]).status.success());
        let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &uri).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "company").unwrap();
    })
    .await
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/api/acme/company/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "branches": [{ "name": "main", "head": HEAD, "status": "open" }]
        })))
        .mount(server)
        .await;
    Checkout {
        _temp: temp,
        dir: checkout_dir,
    }
}

async fn run(dir: &Path, args: &[&str]) -> Output {
    let dir = dir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        oak(&dir, &args)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ls_remote_lists_a_directory_at_the_pinned_head() {
    let server = MockServer::builder().start().await;
    let checkout = linked_checkout(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/acme/company/tree/{HEAD}/libs/auth")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "verify.rs", "path": "libs/auth/verify.rs", "kind": "blob", "blob_hash": "ab" },
            { "name": "src", "path": "libs/auth/src", "kind": "tree", "blob_hash": "" },
            { "name": "README.md", "path": "libs/auth/README.md", "kind": "blob", "blob_hash": "cd" },
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/acme/company/tree/{HEAD}/secret")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let out = run(checkout.path(), &["ls", "--remote", "libs/auth/", "--json"]).await;
    let body = json_stdout(&out);
    assert_eq!(body["commit"], HEAD);
    assert_eq!(body["head_source"], "remote_branch_list");
    assert_eq!(body["verified"], false);
    let names: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["src", "README.md", "verify.rs"],
        "directories first"
    );
    assert!(body["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .starts_with("oak cat --remote libs/auth/README.md"));

    let out = run(
        checkout.path(),
        &[
            "ls",
            "--remote",
            "libs/auth",
            "--max-entries",
            "1",
            "--json",
        ],
    )
    .await;
    let body = json_stdout(&out);
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], true);

    // A directory you can't read looks like one that doesn't exist.
    let out = run(checkout.path(), &["ls", "--remote", "secret"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory you can read"));

    // Without --remote there is nothing to do.
    let out = run(checkout.path(), &["ls", "libs"]).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pass --remote"));
}

#[tokio::test]
async fn grep_remote_narrows_to_paths_and_exits_1_on_no_match() {
    let server = MockServer::builder().start().await;
    let checkout = linked_checkout(&server).await;
    let ndjson = [
        json!({ "type": "meta", "files": 3 }),
        json!({ "type": "file", "path": "libs/auth/verify.rs", "total": 2,
                "lines": [{ "line": 4, "parts": ["pub ", "fn verify", "(t: &str)"] }] }),
        json!({ "type": "file", "path": "apps/web/login.ts", "total": 1,
                "lines": [{ "line": 9, "parts": ["", "fn verify", ""] }] }),
        json!({ "type": "done", "files": 2, "matches": 3, "limited": false }),
    ]
    .iter()
    .map(|v| format!("{v}\n"))
    .collect::<String>();
    Mock::given(method("GET"))
        .and(path("/api/acme/company/search"))
        .and(query_param("q", "fn verify"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ndjson, "application/x-ndjson"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/acme/company/search"))
        .and(query_param("q", "nothing"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("{}\n", json!({ "type": "done", "files": 0, "matches": 0 })),
            "application/x-ndjson",
        ))
        .mount(&server)
        .await;

    let out = run(
        checkout.path(),
        &["grep", "--remote", "fn verify", "--json", "--", "libs/auth"],
    )
    .await;
    let body = json_stdout(&out);
    assert_eq!(body["paths"], json!(["libs/auth"]));
    assert_eq!(body["files"].as_array().unwrap().len(), 1);
    assert_eq!(body["files"][0]["path"], "libs/auth/verify.rs");
    assert_eq!(body["matches"], 2);

    let out = run(checkout.path(), &["grep", "--remote", "fn verify"]).await;
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("libs/auth/verify.rs:4:pub fn verify(t: &str)"),
        "{text}"
    );
    assert!(text.contains("apps/web/login.ts:9:fn verify"), "{text}");

    let out = run(checkout.path(), &["grep", "--remote", "nothing"]).await;
    assert_eq!(out.status.code(), Some(1), "no match exits 1, like grep");
}

/// A view checkout (Projects) sends its view on every request, scoped to its
/// repo: here the branch list and tree listing `oak ls --remote` makes. A
/// full checkout sends none.
#[tokio::test]
async fn a_view_checkout_sends_its_view_on_every_request() {
    use wiremock::matchers::header;
    let server = MockServer::builder().start().await;
    let checkout = linked_checkout(&server).await;
    let spec = "+apps/web\n=AGENTS.md";
    {
        let repo = SqliteRepository::open(&checkout.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::ViewSpec, spec).unwrap();
    }
    // Only a request carrying the scoped view headers gets the listing.
    Mock::given(method("GET"))
        .and(path(format!("/api/acme/company/tree/{HEAD}/apps/web")))
        .and(header("oak-view-repo", "acme/company"))
        .and(header("oak-view-protocol", "view_v1"))
        .and(header("oak-view", "%2Bapps%2Fweb%0A%3DAGENTS.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "name": "main.ts", "path": "apps/web/main.ts", "kind": "blob", "blob_hash": "ab" },
        ])))
        .mount(&server)
        .await;
    let out = run(checkout.path(), &["ls", "--remote", "apps/web", "--json"]).await;
    let body = json_stdout(&out);
    assert_eq!(body["entries"][0]["name"], "main.ts");

    // Clearing the view: the same request no longer matches the mock.
    {
        let repo = SqliteRepository::open(&checkout.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::ViewSpec, "").unwrap();
    }
    let out = run(checkout.path(), &["ls", "--remote", "apps/web", "--json"]).await;
    assert!(
        !out.status.success(),
        "a full checkout sends no view headers"
    );
}

/// `oak view`: a full checkout has no view to show or change; a view
/// checkout shows its spec, refuses to drop what it doesn't hold, and refuses
/// to change while it has local work it could lose.
#[tokio::test]
async fn oak_view_shows_the_view_and_guards_changes() {
    let server = MockServer::builder().start().await;
    let checkout = linked_checkout(&server).await;
    let out = run(checkout.path(), &["view", "show", "--json"]).await;
    assert_eq!(
        out.status.code(),
        Some(2),
        "a full checkout is a usage error"
    );

    {
        let repo = SqliteRepository::open(&checkout.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::ViewSpec, "+apps/web\n+libs/ui\n=AGENTS.md")
            .unwrap();
    }
    let out = run(checkout.path(), &["view", "show", "--json"]).await;
    let body = json_stdout(&out);
    assert_eq!(body["repo"], "acme/company");
    assert_eq!(body["include"], json!(["apps/web", "libs/ui"]));
    assert_eq!(body["spine_files"], json!(["AGENTS.md"]));

    let out = run(checkout.path(), &["view", "remove", "apps/billing"]).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("isn't in this view"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::write(checkout.path().join("scratch.txt"), "local work\n").unwrap();
    let out = run(checkout.path(), &["view", "add", "apps/billing"]).await;
    assert_eq!(
        out.status.code(),
        Some(4),
        "dirty work blocks a view change"
    );
}
