//! `oak root` against a fake roots API that behaves like oak-web's
//! (`server::api::roots` + `root_content`): a linear head CAS on push, pulls
//! that return the head commit and only the trees `since` doesn't reach, and
//! blobs by hash. Covers the round trip between two checkouts, sending only
//! new content, `head_moved`, three-way pulls and conflicts, the checkout
//! ignoring `roots/NAME/`, a root started from a `main` commit, and the
//! list/show/create/archive wrappers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use oak_core::protocol::TreeData;
use oak_core::{
    Blob, Commit, FileMode, Hash, ManifestEntry, MetadataKey, Repository, SqliteRepository, Tree,
    TreeEntryKind,
};
use serde_json::{json, Value};
use wiremock::matchers::path_regex;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Default)]
struct RootsState {
    /// The one root this server holds, as `~name` (default `~ops`).
    name: String,
    owner_group: Option<String>,
    write_mode: String,
    base_commit: Option<String>,
    /// `write` unless a test narrows it.
    access: String,
    head: Option<String>,
    /// hash → (parent, manifest, wire commit)
    commits: HashMap<String, (Option<String>, String, Value)>,
    trees: HashMap<String, Tree>,
    blobs: HashMap<String, Vec<u8>>,
    /// Blob hashes each push carried, in order.
    pushed_blobs: Vec<Vec<String>>,
    archived: bool,
    /// Bodies of `POST roots`.
    create_bodies: Vec<Value>,
    /// Requests of any kind, for checking a command never reached the server.
    requests: usize,
}

impl RootsState {
    fn new() -> Self {
        RootsState {
            name: "~ops".to_string(),
            owner_group: Some("ops".to_string()),
            write_mode: "direct".to_string(),
            access: "write".to_string(),
            ..RootsState::default()
        }
    }

    fn root_json(&self) -> Value {
        let mut doc = json!({
            "name": self.name,
            "write_mode": self.write_mode,
            "created_at": "2026-11-09T00:00:00Z",
            "access": self.access,
            "can_archive": true,
        });
        if let Some(group) = &self.owner_group {
            doc["owner_group"] = json!(group);
        }
        if let Some(base) = &self.base_commit {
            doc["base_commit"] = json!(base);
        }
        if self.access != "metadata" {
            if let Some(head) = &self.head {
                doc["head"] = json!(head);
            }
        }
        if self.archived {
            doc["archived_at"] = json!("2026-11-09T01:00:00Z");
        }
        doc
    }

    /// Seed the root as one started from a `main` commit: that commit's
    /// parent is a `main` commit the root doesn't hold.
    fn start_from_main(&mut self, files: &[(&str, &[u8])]) -> String {
        let entries: Vec<ManifestEntry> = files
            .iter()
            .map(|(path, content)| {
                let blob = Blob::new(content.to_vec());
                self.blobs.insert(blob.hash.to_string(), content.to_vec());
                ManifestEntry {
                    path: path.to_string(),
                    blob_hash: blob.hash,
                    mode: FileMode::Regular,
                }
            })
            .collect();
        let built = oak_core::tree::build_tree(&entries).unwrap();
        for tree in built.trees {
            self.trees.insert(tree.hash.to_string(), tree);
        }
        let main_parent = Hash::from_hex(&"7".repeat(64)).unwrap();
        let commit = Commit::new(
            "main".to_string(),
            Some(main_parent.clone()),
            None,
            built.root_hash.clone(),
            "zach".to_string(),
            Some("Release 1.0".to_string()),
            Vec::new(),
        )
        .unwrap();
        let hash = commit.hash.to_string();
        let wire = json!({
            "hash": hash,
            "branch_name": "main",
            "parent_hash": main_parent.to_string(),
            "merge_parent_hash": null,
            "manifest_hash": built.root_hash.to_string(),
            "author": commit.author,
            "message": commit.message,
            "timestamp": commit.timestamp.to_rfc3339(),
            "files": [],
        });
        self.commits.insert(
            hash.clone(),
            (
                Some(main_parent.to_string()),
                built.root_hash.to_string(),
                wire,
            ),
        );
        self.base_commit = Some(hash.clone());
        self.head = Some(hash.clone());
        hash
    }

    fn closure(&self, manifest: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut stack = vec![manifest.to_string()];
        while let Some(h) = stack.pop() {
            let Some(tree) = self.trees.get(&h) else {
                continue;
            };
            if !out.insert(h) {
                continue;
            }
            for e in &tree.entries {
                if e.kind == TreeEntryKind::Tree {
                    stack.push(e.hash.to_string());
                }
            }
        }
        out
    }

    fn blobs_of(&self, manifest: &str) -> Vec<String> {
        self.closure(manifest)
            .iter()
            .flat_map(|t| self.trees[t].entries.iter())
            .filter(|e| e.kind == TreeEntryKind::Blob)
            .map(|e| e.hash.to_string())
            .collect()
    }
}

fn refusal(status: u16, code: &str, error: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "error": error, "code": code }))
}

struct FakeRoots(Arc<Mutex<RootsState>>);

impl Respond for FakeRoots {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let mut st = self.0.lock().unwrap();
        st.requests += 1;
        let path = urlencoding::decode(req.url.path()).unwrap().into_owned();
        let rest = path.strip_prefix("/api/acme/web/roots").unwrap();
        let query: HashMap<String, String> = req.url.query_pairs().into_owned().collect();
        let method = req.method.as_str();
        let me = format!("/{}", st.name);
        let sub = rest.strip_prefix(me.as_str()).map(str::to_string);
        match (method, rest, sub.as_deref()) {
            ("GET", "", _) => ResponseTemplate::new(200).set_body_json(json!({
                "roots": [st.root_json()],
            })),
            ("POST", "", _) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                st.create_bodies.push(body.clone());
                st.name = format!("~{}", body["name"].as_str().unwrap());
                st.owner_group = body["owner_group"].as_str().map(str::to_string);
                st.write_mode = body["write_mode"].as_str().unwrap_or("direct").to_string();
                ResponseTemplate::new(201).set_body_json(json!({ "root": st.root_json() }))
            }
            ("GET", _, Some("")) => ResponseTemplate::new(200).set_body_json(st.root_json()),
            ("DELETE", _, Some("")) => {
                st.archived = true;
                ResponseTemplate::new(200).set_body_json(st.root_json())
            }
            ("POST", _, Some("/push")) if st.archived => refusal(404, "not_found", "Not found"),
            ("POST", _, Some("/push")) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let expected = body["expected_head"].as_str().map(str::to_string);
                if expected != st.head {
                    return refusal(409, "head_moved", "the root moved since you last pulled");
                }
                let mut sent = Vec::new();
                for b in body["blobs"].as_array().unwrap() {
                    let content: Vec<u8> = serde_json::from_value(b["content"].clone()).unwrap();
                    let hash = Blob::new(content.clone()).hash.to_string();
                    assert_eq!(b["hash"].as_str().unwrap(), hash);
                    assert!(content.len() <= 4 * 1024 * 1024);
                    sent.push(hash.clone());
                    st.blobs.insert(hash, content);
                }
                st.pushed_blobs.push(sent);
                for t in body["trees"].as_array().unwrap() {
                    let td: TreeData = serde_json::from_value(t.clone()).unwrap();
                    let tree = oak_core::protocol::tree_data_to_core(&td).unwrap();
                    st.trees.insert(tree.hash.to_string(), tree);
                }
                let mut parent = expected;
                for c in body["commits"].as_array().unwrap() {
                    assert_eq!(c["branch_name"], st.name.as_str());
                    let cd: oak_core::protocol::CommitData =
                        serde_json::from_value(c.clone()).unwrap();
                    let commit = oak_core::protocol::commit_data_to_core(&cd).unwrap();
                    assert_eq!(commit.parent_hash.map(|h| h.to_string()), parent);
                    let manifest = commit.manifest_hash.to_string();
                    // Every blob the tree names must now be held.
                    let empty = Blob::new(Vec::new()).hash.to_string();
                    for blob in st.blobs_of(&manifest) {
                        if blob != empty && !st.blobs.contains_key(&blob) {
                            return refusal(422, "missing_blob", &format!("blob {blob} missing"));
                        }
                    }
                    let hash = commit.hash.to_string();
                    st.commits
                        .insert(hash.clone(), (parent.clone(), manifest, c.clone()));
                    parent = Some(hash);
                }
                let previous = st.head.clone();
                st.head = parent.clone();
                ResponseTemplate::new(200).set_body_json(json!({
                    "root": st.name,
                    "head": parent,
                    "previous_head": previous,
                    "commits": body["commits"].as_array().unwrap().len(),
                }))
            }
            ("GET", _, Some("/pull")) if st.access == "metadata" => refusal(
                403,
                "forbidden",
                "this root started from main and holds every file there, so reading it takes being able to read every path of the repo",
            ),
            ("GET", _, Some("/pull")) => {
                let Some(head) = st.head.clone() else {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "root": st.name, "commits": [], "trees": [], "blobs": [], "complete": true
                    }));
                };
                let since = query.get("since").cloned();
                if let Some(s) = &since {
                    if !st.commits.contains_key(s) {
                        return refusal(422, "invalid_since", "since isn't a commit of this root");
                    }
                }
                let skip = since
                    .as_ref()
                    .map(|s| st.closure(&st.commits[s].1))
                    .unwrap_or_default();
                let (commits, trees) = if since.as_deref() == Some(head.as_str()) {
                    (vec![], vec![])
                } else {
                    let manifest = st.commits[&head].1.clone();
                    let trees: Vec<Value> = st
                        .closure(&manifest)
                        .difference(&skip)
                        .map(|h| {
                            serde_json::to_value(oak_core::protocol::tree_to_wire(&st.trees[h]))
                                .unwrap()
                        })
                        .collect();
                    (vec![st.commits[&head].2.clone()], trees)
                };
                // Like the server with `limit=1`: only the head commit, so a
                // longer history is never complete; a root from main is
                // complete once the walk reaches its base commit.
                let complete = commits.is_empty()
                    || st.base_commit.as_deref() == Some(head.as_str())
                    || st.commits[&head].0.is_none();
                ResponseTemplate::new(200).set_body_json(json!({
                    "root": st.name,
                    "head": head,
                    "commits": commits,
                    "trees": trees,
                    "blobs": [],
                    "complete": complete,
                }))
            }
            ("GET", _, Some(blob)) if blob.starts_with("/blobs/") => {
                let hash = blob.trim_start_matches("/blobs/");
                match st.blobs.get(hash) {
                    Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                    None if hash == Blob::new(Vec::new()).hash.as_str() => {
                        ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new())
                    }
                    None => refusal(404, "not_found", "Not found"),
                }
            }
            _ => refusal(404, "not_found", "Not found"),
        }
    }
}

fn oak(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_WATCH", "0")
        .env("HOME", home)
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn ok_json(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: std::path::PathBuf,
    a: std::path::PathBuf,
    b: std::path::PathBuf,
    state: Arc<Mutex<RootsState>>,
    _server: MockServer,
}

/// Two linked acme/web checkouts against one fake roots server.
async fn fixture() -> Fixture {
    let server = MockServer::start().await;
    let state = Arc::new(Mutex::new(RootsState::new()));
    Mock::given(path_regex(r"^/api/acme/web/roots"))
        .respond_with(FakeRoots(state.clone()))
        .mount(&server)
        .await;
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut dirs = Vec::new();
    for name in ["a", "b"] {
        let dir = temp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let init = oak(&dir, &home, &["init", "."]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let repo = SqliteRepository::open(&dir.join(".oak").join("oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "web").unwrap();
        dirs.push(dir);
    }
    let b = dirs.pop().unwrap();
    let a = dirs.pop().unwrap();
    Fixture {
        _temp: temp,
        home,
        a,
        b,
        state,
        _server: server,
    }
}

#[tokio::test]
async fn push_then_pull_round_trips_and_sends_only_new_content() {
    let f = fixture().await;

    // First pull of an empty root: roots/ops/ appears and is ignored.
    let doc = ok_json(&oak(&f.a, &f.home, &["root", "pull", "ops", "--json"]));
    assert_eq!(doc["root"], "~ops");
    assert_eq!(doc["ignored_by_checkout"], "roots/ops");
    assert!(doc["head"].is_null());
    assert!(f.a.join("roots/ops/.oakroot").is_file());
    assert_eq!(
        std::fs::read_to_string(f.a.join(".oak/root-dirs")).unwrap(),
        "roots/ops\n"
    );

    let ops = f.a.join("roots/ops");
    std::fs::create_dir_all(ops.join("bin")).unwrap();
    std::fs::write(ops.join("notes.md"), "# notes\n").unwrap();
    std::fs::write(ops.join("bin/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(ops.join("empty.txt"), "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            ops.join("bin/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let doc = ok_json(&oak(&f.a, &f.home, &["root", "push", "ops", "--json"]));
    assert_eq!(doc["pushed"], true);
    assert_eq!(doc["blobs_sent"], 2, "the empty file's blob is never sent");
    let head = doc["head"].as_str().unwrap().to_string();
    assert_eq!(f.state.lock().unwrap().head.as_deref(), Some(head.as_str()));

    // The checkout never sees roots/ops/.
    let status = ok_json(&oak(&f.a, &f.home, &["status", "--json"]));
    let changes = status["changes"].as_array().unwrap();
    assert!(
        changes
            .iter()
            .all(|c| !c["path"].as_str().unwrap().starts_with("roots")),
        "roots/ops leaked into the checkout: {changes:?}"
    );

    // Nothing changed: nothing to push (from inside the directory, no name).
    let doc = ok_json(&oak(&ops, &f.home, &["root", "push", "--json"]));
    assert_eq!(doc["pushed"], false);

    // Another checkout pulls the same files.
    let doc = ok_json(&oak(&f.b, &f.home, &["root", "pull", "~ops", "--json"]));
    assert_eq!(doc["head"], head.as_str());
    let b_ops = f.b.join("roots/ops");
    assert_eq!(
        std::fs::read_to_string(b_ops.join("notes.md")).unwrap(),
        "# notes\n"
    );
    assert_eq!(std::fs::read(b_ops.join("empty.txt")).unwrap(), b"");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(b_ops.join("bin/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "executable bit survives the round trip");
    }

    // Edit one file from inside the root directory: only its blob is sent.
    std::fs::write(b_ops.join("notes.md"), "# notes\nmore\n").unwrap();
    let doc = ok_json(&oak(&b_ops, &f.home, &["root", "push", "--json"]));
    assert_eq!(doc["pushed"], true);
    assert_eq!(doc["blobs_sent"], 1);
    assert_eq!(doc["changed_files"], 1);

    // A file removed upstream is removed here.
    std::fs::remove_file(b_ops.join("bin/run.sh")).unwrap();
    ok_json(&oak(&b_ops, &f.home, &["root", "push", "--json"]));
    let doc = ok_json(&oak(&f.a, &f.home, &["root", "pull", "ops", "--json"]));
    assert_eq!(doc["written"], json!(["notes.md"]));
    assert_eq!(doc["deleted"], json!(["bin/run.sh"]));
    assert!(!ops.join("bin").exists(), "emptied directories are pruned");
    assert_eq!(
        std::fs::read_to_string(ops.join("notes.md")).unwrap(),
        "# notes\nmore\n"
    );
    assert_eq!(f.state.lock().unwrap().pushed_blobs.len(), 3);
}

#[tokio::test]
async fn pull_and_push_need_a_root_name_outside_a_root_directory() {
    let f = fixture().await;
    for verb in ["pull", "push"] {
        let out = oak(&f.a, &f.home, &["root", verb, "--json"]);
        assert_eq!(out.status.code(), Some(2), "{verb}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["error"]["code"], "invalid_argument");
        assert!(doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("oak root list"));
    }
    assert_eq!(f.state.lock().unwrap().requests, 0);
}

#[tokio::test]
async fn a_root_started_from_main_pulls_its_base_and_pushes_only_new_content() {
    let f = fixture().await;
    let base = {
        let mut st = f.state.lock().unwrap();
        st.name = "~stable".to_string();
        st.owner_group = Some("release".to_string());
        st.start_from_main(&[
            ("README.md", b"# web\n"),
            ("src/lib.rs", b"pub fn hi() {}\n"),
            (".oak/workflows/ci.yml", b"on: [push]\n"),
        ])
    };

    // Its first commit is main's, whose parent is outside the root: that is
    // where the root's history starts.
    let doc = ok_json(&oak(&f.a, &f.home, &["root", "pull", "stable", "--json"]));
    assert_eq!(doc["head"], base.as_str());
    let dir = f.a.join("roots/stable");
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "pub fn hi() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".oak/workflows/ci.yml")).unwrap(),
        "on: [push]\n"
    );

    // The checkout ignores it, `.oak/` inside it included.
    let status = ok_json(&oak(&f.a, &f.home, &["status", "--json"]));
    assert!(
        status["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| !c["path"].as_str().unwrap().starts_with("roots")),
        "{status}"
    );

    // A patch on the line: one new blob, main's files aren't sent again, and
    // the workflow file stays in the root's tree.
    std::fs::write(dir.join("src/lib.rs"), "pub fn hi() { fix() }\n").unwrap();
    let doc = ok_json(&oak(&dir, &f.home, &["root", "push", "--json"]));
    assert_eq!(doc["pushed"], true);
    assert_eq!(doc["previous_head"], base.as_str());
    assert_eq!(doc["blobs_sent"], 1);
    assert_eq!(doc["changed_files"], 1);
    let head = doc["head"].as_str().unwrap().to_string();
    {
        let st = f.state.lock().unwrap();
        let (parent, manifest, _) = &st.commits[&head];
        assert_eq!(parent.as_deref(), Some(base.as_str()));
        assert_eq!(st.blobs_of(manifest).len(), 3);
    }

    // Pulling again only moves forward from the base.
    let doc = ok_json(&oak(&f.b, &f.home, &["root", "pull", "stable", "--json"]));
    assert_eq!(doc["head"], head.as_str());
    assert_eq!(
        std::fs::read_to_string(f.b.join("roots/stable/src/lib.rs")).unwrap(),
        "pub fn hi() { fix() }\n"
    );

    // A directory that never pulled can't push onto main's files.
    let fresh = f.b.join("elsewhere");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(fresh.join("a.txt"), "a\n").unwrap();
    let out = oak(
        &f.b,
        &f.home,
        &[
            "root",
            "push",
            "stable",
            "--dir",
            fresh.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(5));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["error"]["code"], "pull_required");
}

#[tokio::test]
async fn a_root_from_main_you_cant_fully_read_shows_but_wont_pull() {
    let f = fixture().await;
    {
        let mut st = f.state.lock().unwrap();
        st.name = "~stable".to_string();
        st.start_from_main(&[("README.md", b"# web\n")]);
        st.access = "metadata".to_string();
    }
    let out = oak(&f.a, &f.home, &["root", "show", "stable"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("from main at"), "{text}");
    assert!(text.contains("(can't read)"), "{text}");

    let out = oak(&f.a, &f.home, &["root", "pull", "stable"]);
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("every path of the repo"));
    assert!(!f.a.join("roots/stable").exists());
}

#[tokio::test]
async fn a_stale_push_says_pull_and_pull_keeps_local_edits() {
    let f = fixture().await;
    let a_ops = f.a.join("roots/ops");
    let b_ops = f.b.join("roots/ops");
    std::fs::create_dir_all(&a_ops).unwrap();
    std::fs::write(a_ops.join("a.txt"), "a1\n").unwrap();
    std::fs::write(a_ops.join("b.txt"), "b1\n").unwrap();
    ok_json(&oak(&f.a, &f.home, &["root", "push", "ops", "--json"]));
    ok_json(&oak(&f.b, &f.home, &["root", "pull", "ops", "--json"]));

    // B moves the root.
    std::fs::write(b_ops.join("a.txt"), "a2\n").unwrap();
    ok_json(&oak(&b_ops, &f.home, &["root", "push", "--json"]));

    // A edits a different file and pushes: head_moved, exit 5.
    std::fs::write(a_ops.join("b.txt"), "b2\n").unwrap();
    let out = oak(&a_ops, &f.home, &["root", "push", "--json"]);
    assert_eq!(out.status.code(), Some(5));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["error"]["code"], "head_moved");
    assert!(doc["error"]["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .starts_with("oak root pull"));

    // Pull takes B's a.txt and keeps A's b.txt; then the push lands.
    let doc = ok_json(&oak(&a_ops, &f.home, &["root", "pull", "--json"]));
    assert_eq!(doc["written"], json!(["a.txt"]));
    assert_eq!(doc["kept_local"], json!(["b.txt"]));
    assert_eq!(
        std::fs::read_to_string(a_ops.join("a.txt")).unwrap(),
        "a2\n"
    );
    assert_eq!(
        std::fs::read_to_string(a_ops.join("b.txt")).unwrap(),
        "b2\n"
    );
    let doc = ok_json(&oak(&a_ops, &f.home, &["root", "push", "--json"]));
    assert_eq!(doc["pushed"], true);
}

#[tokio::test]
async fn a_file_changed_on_both_sides_refuses_the_pull_until_forced() {
    let f = fixture().await;
    let a_ops = f.a.join("roots/ops");
    let b_ops = f.b.join("roots/ops");
    std::fs::create_dir_all(&a_ops).unwrap();
    std::fs::write(a_ops.join("a.txt"), "base\n").unwrap();
    ok_json(&oak(&f.a, &f.home, &["root", "push", "ops", "--json"]));
    ok_json(&oak(&f.b, &f.home, &["root", "pull", "ops", "--json"]));
    std::fs::write(b_ops.join("a.txt"), "theirs\n").unwrap();
    ok_json(&oak(&b_ops, &f.home, &["root", "push", "--json"]));

    std::fs::write(a_ops.join("a.txt"), "mine\n").unwrap();
    let out = oak(&a_ops, &f.home, &["root", "pull", "--json"]);
    assert_eq!(out.status.code(), Some(5));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["error"]["code"], "root_conflict");
    assert_eq!(doc["error"]["conflict_paths"], json!(["a.txt"]));
    assert_eq!(
        std::fs::read_to_string(a_ops.join("a.txt")).unwrap(),
        "mine\n"
    );

    let doc = ok_json(&oak(
        &a_ops,
        &f.home,
        &["root", "pull", "--force", "--json"],
    ));
    assert_eq!(doc["overwritten_conflicts"], json!(["a.txt"]));
    assert_eq!(
        std::fs::read_to_string(a_ops.join("a.txt")).unwrap(),
        "theirs\n"
    );
}

#[tokio::test]
async fn pushing_without_state_to_a_root_with_history_asks_for_a_pull() {
    let f = fixture().await;
    std::fs::create_dir_all(f.a.join("roots/ops")).unwrap();
    std::fs::write(f.a.join("roots/ops/a.txt"), "a\n").unwrap();
    ok_json(&oak(&f.a, &f.home, &["root", "push", "ops", "--json"]));

    std::fs::create_dir_all(f.b.join("roots/ops")).unwrap();
    std::fs::write(f.b.join("roots/ops/a.txt"), "other\n").unwrap();
    let out = oak(&f.b, &f.home, &["root", "push", "ops", "--json"]);
    assert_eq!(out.status.code(), Some(5));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["error"]["code"], "pull_required");
}

#[tokio::test]
async fn list_show_create_and_archive_wrap_the_api() {
    let f = fixture().await;

    let doc = ok_json(&oak(&f.a, &f.home, &["root", "list", "--json"]));
    assert_eq!(doc["roots"][0]["name"], "~ops");

    let out = oak(&f.a, &f.home, &["root", "list"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("~ops"), "{text}");
    assert!(text.contains("@group/ops"), "{text}");

    // Create sends the general shape: a name, and only what was asked for.
    let doc = ok_json(&oak(
        &f.a,
        &f.home,
        &[
            "root",
            "create",
            "stable",
            "--from",
            "main",
            "--owner-group",
            "release",
            "--json",
        ],
    ));
    assert_eq!(doc["root"]["name"], "~stable");
    let full = "ab".repeat(32);
    ok_json(&oak(
        &f.a,
        &f.home,
        &["root", "create", "~scratch", "--sync", "--json"],
    ));
    ok_json(&oak(
        &f.a,
        &f.home,
        &["root", "create", "hotfix", "--from", &full, "--json"],
    ));
    assert_eq!(
        f.state.lock().unwrap().create_bodies,
        vec![
            json!({ "name": "stable", "base_commit": "main", "owner_group": "release" }),
            json!({ "name": "scratch", "write_mode": "sync" }),
            json!({ "name": "hotfix", "base_commit": full }),
        ]
    );

    // A bad --from never reaches the server.
    let before = f.state.lock().unwrap().requests;
    let out = oak(
        &f.a,
        &f.home,
        &["root", "create", "x", "--from", "abc123", "--json"],
    );
    assert_eq!(out.status.code(), Some(2));
    let out = oak(&f.a, &f.home, &["root", "create", "--json"]);
    assert_eq!(out.status.code(), Some(2), "a name is required");
    assert_eq!(f.state.lock().unwrap().requests, before);

    let doc = ok_json(&oak(&f.a, &f.home, &["root", "show", "hotfix", "--json"]));
    assert_eq!(doc["write_mode"], "direct");

    // Archiving needs --yes off a terminal, and never calls the server first.
    let out = oak(&f.a, &f.home, &["root", "archive", "~hotfix"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(!f.state.lock().unwrap().archived);

    let doc = ok_json(&oak(
        &f.a,
        &f.home,
        &["root", "archive", "~hotfix", "--yes", "--json"],
    ));
    assert!(doc["archived_at"].is_string());

    let out = oak(&f.a, &f.home, &["root", "show", "nobody"]);
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Not found"));
}

#[tokio::test]
async fn pushing_an_archived_root_fails() {
    let f = fixture().await;
    let ops = f.a.join("roots/ops");
    std::fs::create_dir_all(&ops).unwrap();
    std::fs::write(ops.join("a.txt"), "a\n").unwrap();
    ok_json(&oak(&f.a, &f.home, &["root", "push", "ops", "--json"]));
    f.state.lock().unwrap().archived = true;

    std::fs::write(ops.join("a.txt"), "b\n").unwrap();
    let out = oak(&ops, &f.home, &["root", "push"]);
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("archived"));
    assert_eq!(f.state.lock().unwrap().create_bodies, Vec::<Value>::new());
}
