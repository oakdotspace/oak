use oak_core::{
    Branch, Commit, FileMode, Hash, Manifest, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Fixture {
    dir: tempfile::TempDir,
    repo: SqliteRepository,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".oak")).unwrap();
        let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
        Self { dir, repo }
    }
    fn commit(&self, name: &str, parent: Option<Hash>, files: &[(&str, &[u8])]) -> Commit {
        let entries = files
            .iter()
            .map(|(path, bytes)| ManifestEntry {
                path: (*path).into(),
                blob_hash: self.repo.put_blob(bytes.to_vec()).unwrap(),
                mode: FileMode::Regular,
            })
            .collect();
        let manifest = Manifest::new(entries);
        self.repo.store_manifest(&manifest).unwrap();
        let commit = Commit::with_timestamp(
            name.into(),
            parent,
            None,
            manifest.hash,
            "qa".into(),
            None,
            vec![],
            chrono::Utc::now(),
        )
        .unwrap();
        self.repo.store_commit(&commit).unwrap();
        self.repo
            .store_branch(&Branch::new(
                name.into(),
                None,
                if name == "main" {
                    None
                } else {
                    Some("main".into())
                },
            ))
            .unwrap();
        self.repo.set_branch_head(name, &commit.hash).unwrap();
        commit
    }
    fn select(&self, commit: &Commit) {
        self.repo.set_current_branch(&commit.branch_name).unwrap();
        self.repo.set_head(&commit.hash).unwrap();
    }
}

fn invoke(dir: &Path, names: &[&str], remote: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .current_dir(dir)
        .args(["branch", "train", "--json"])
        .args(names)
        .env("OAK_NO_UPDATE_CHECK", "1");
    if remote {
        command.arg("--remote");
    }
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (output, json)
}

fn body(edits: &[(usize, &str)]) -> Vec<u8> {
    let mut lines: Vec<_> = (0..30).map(|n| format!("line {n}\n")).collect();
    for (n, value) in edits {
        lines[*n] = format!("{value}\n");
    }
    lines.concat().into_bytes()
}

fn tree(files: &[(&str, &[u8])]) -> Hash {
    oak_core::tree::build_tree(
        &files
            .iter()
            .map(|(path, bytes)| ManifestEntry {
                path: (*path).into(),
                blob_hash: oak_core::hash_bytes(bytes),
                mode: FileMode::Regular,
            })
            .collect::<Vec<_>>(),
    )
    .unwrap()
    .root_hash
}

#[test]
fn train_carries_merged_bytes_across_three_sibling_steps_and_preserves_dirty_sparse_checkout() {
    let f = Fixture::new();
    let root = f.commit(
        "main",
        None,
        &[
            ("file", &body(&[])),
            ("outside/binary", &[0, 255]),
            ("empty", b""),
        ],
    );
    let a = f.commit(
        "a",
        Some(root.hash.clone()),
        &[
            ("file", &body(&[(2, "A")])),
            ("outside/binary", &[0, 255]),
            ("empty", b""),
        ],
    );
    f.commit(
        "b",
        Some(root.hash.clone()),
        &[
            ("file", &body(&[(12, "B")])),
            ("outside/binary", &[0, 255]),
            ("empty", b""),
        ],
    );
    f.commit(
        "c",
        Some(root.hash.clone()),
        &[
            ("file", &body(&[(22, "C")])),
            ("outside/binary", &[0, 255]),
            ("empty", b""),
        ],
    );
    f.select(&a);
    std::fs::write(f.dir.path().join("file"), b"dirty user bytes").unwrap();
    std::fs::write(f.dir.path().join("untracked"), b"leave me").unwrap();
    f.repo
        .set_metadata(MetadataKey::SparsePaths, "[\"file\"]")
        .unwrap();
    let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    conn.execute(
        "DELETE FROM blobs WHERE hash=?",
        [oak_core::hash_bytes(&[]).as_str()],
    )
    .unwrap();
    let counts = || -> Vec<i64> {
        ["blobs", "trees", "commits"]
            .iter()
            .map(|table| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap()
            })
            .collect()
    };
    let before = counts();
    let (output, json) = invoke(f.dir.path(), &["a", "b", "c"], false);
    assert!(output.status.success(), "{json}");
    let expected = tree(&[
        ("file", &body(&[(2, "A"), (12, "B"), (22, "C")])),
        ("outside/binary", &[0, 255]),
        ("empty", b""),
    ]);
    assert_eq!(json["candidate_tree"], expected.to_string());
    assert_eq!(
        json["steps"][2]["input_tree"],
        json["steps"][1]["candidate_tree"]
    );
    assert_eq!(json["steps"][0]["fork"], root.hash.to_string());
    assert_eq!(json["steps"].as_array().unwrap().len(), 3);
    assert_eq!(counts(), before);
    assert_eq!(f.repo.get_head().unwrap(), Some(a.hash));
    assert_eq!(
        std::fs::read(f.dir.path().join("file")).unwrap(),
        b"dirty user bytes"
    );
    assert_eq!(
        std::fs::read(f.dir.path().join("untracked")).unwrap(),
        b"leave me"
    );
    assert!(!f.dir.path().join("outside").exists());
    let (_, reordered) = invoke(f.dir.path(), &["c", "b", "a"], false);
    assert_eq!(reordered["candidate_tree"], expected.to_string());
}

#[test]
fn train_stops_on_ordered_overlap_even_when_each_branch_is_pairwise_clean() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[("file", &body(&[]))]);
    let a = f.commit(
        "a",
        Some(root.hash.clone()),
        &[("file", &body(&[(2, "A")]))],
    );
    f.commit(
        "b",
        Some(root.hash.clone()),
        &[("file", &body(&[(2, "B")]))],
    );
    f.commit("c", Some(root.hash), &[("file", &body(&[(22, "C")]))]);
    f.select(&a);
    for names in [["a", "b", "c"], ["b", "a", "c"]] {
        let (output, json) = invoke(f.dir.path(), &names, false);
        assert!(!output.status.success());
        assert_eq!(json["state"], "conflict");
        assert_eq!(json["steps"].as_array().unwrap().len(), 2);
        assert_eq!(json["steps"][1]["conflict_files"][0]["path"], "file");
        assert!(json["candidate_tree"].is_null());
        assert!(json["steps"][1]["candidate_tree"].is_null());
    }
}

#[test]
fn train_rejects_stacked_and_shared_unlanded_ancestry_in_both_orders() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[("file", b"base")]);
    let a = f.commit("a", Some(root.hash), &[("file", b"A")]);
    f.commit("b", Some(a.hash.clone()), &[("file", b"B")]);
    f.commit("c", Some(a.hash.clone()), &[("file", b"C")]);
    f.select(&a);
    for names in [["a", "b"], ["b", "a"], ["b", "c"], ["c", "b"]] {
        let (output, json) = invoke(f.dir.path(), &names, false);
        assert!(!output.status.success());
        assert_eq!(json["state"], "unsupported");
        assert!(json["steps"].as_array().unwrap().is_empty());
    }
}

#[test]
fn train_preserves_disjoint_rename_empty_and_binary_additions() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[("old", b"rename me"), ("empty", b"")]);
    let a = f.commit(
        "a",
        Some(root.hash.clone()),
        &[("new", b"rename me"), ("empty", b"")],
    );
    f.commit(
        "b",
        Some(root.hash),
        &[
            ("old", b"rename me"),
            ("empty", b""),
            ("binary", &[255, 0, 42]),
        ],
    );
    f.select(&a);
    let (output, json) = invoke(f.dir.path(), &["a", "b"], false);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["candidate_tree"],
        tree(&[
            ("new", b"rename me"),
            ("empty", b""),
            ("binary", &[255, 0, 42])
        ])
        .to_string()
    );
}

#[test]
fn train_accepts_siblings_forked_at_different_target_commits() {
    let f = Fixture::new();
    let old = f.commit("main", None, &[("file", &body(&[]))]);
    let target = f.commit(
        "main",
        Some(old.hash.clone()),
        &[("file", &body(&[(6, "TARGET")]))],
    );
    let a = f.commit("a", Some(old.hash.clone()), &[("file", &body(&[(2, "A")]))]);
    f.commit(
        "b",
        Some(target.hash.clone()),
        &[("file", &body(&[(6, "TARGET"), (22, "B")]))],
    );
    f.select(&a);
    let (output, json) = invoke(f.dir.path(), &["a", "b"], false);
    assert!(output.status.success(), "{json}");
    assert_eq!(json["steps"][0]["fork"], old.hash.to_string());
    assert_eq!(json["steps"][1]["fork"], target.hash.to_string());
    assert_eq!(
        json["candidate_tree"],
        tree(&[("file", &body(&[(2, "A"), (6, "TARGET"), (22, "B")]))]).to_string()
    );
}

#[test]
fn train_empty_target_and_declared_root_have_explicit_tree_and_base_identity() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[]);
    let a = f.commit("a", None, &[("file", b"root-seeded")]);
    f.select(&a);
    let (output, json) = invoke(f.dir.path(), &["a"], false);
    assert!(output.status.success(), "{json}");
    assert_eq!(json["initial_tree"], Manifest::empty().hash.to_string());
    assert_eq!(json["against"]["head"], root.hash.to_string());
    assert_eq!(json["steps"][0]["fork_kind"], "declared_root");
    assert!(json["steps"][0]["fork"].is_null());
}

#[test]
fn train_rejects_duplicate_and_over_budget_branch_inputs_before_reading_objects() {
    let f = Fixture::new();
    let distinct: Vec<String> = (0..33).map(|n| format!("branch-{n}")).collect();
    for names in [
        vec!["a", "a"],
        distinct.iter().map(String::as_str).collect(),
        vec!["main"],
    ] {
        let (output, json) = invoke(f.dir.path(), &names, false);
        assert!(!output.status.success());
        assert_eq!(json["error"]["code"], "invalid_argument");
    }
}

#[test]
fn train_proves_siblings_above_missing_history_below_their_exact_common_base() {
    let f = Fixture::new();
    let older = f.commit("main", None, &[("file", &body(&[]))]);
    let target = f.commit("main", Some(older.hash.clone()), &[("file", &body(&[]))]);
    let a = f.commit(
        "a",
        Some(target.hash.clone()),
        &[("file", &body(&[(2, "A")]))],
    );
    f.commit(
        "b",
        Some(target.hash.clone()),
        &[("file", &body(&[(22, "B")]))],
    );
    f.select(&a);
    let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    conn.execute("DELETE FROM commits WHERE hash=?", [older.hash.as_str()])
        .unwrap();
    for names in [["a", "b"], ["b", "a"]] {
        let (output, json) = invoke(f.dir.path(), &names, false);
        assert!(output.status.success(), "{json}");
        assert_eq!(json["state"], "predicted");
        assert_eq!(json["steps"][0]["fork"], target.hash.to_string());
        assert_eq!(json["steps"][1]["fork"], target.hash.to_string());
        assert_eq!(
            json["candidate_tree"],
            tree(&[("file", &body(&[(2, "A"), (22, "B")]))]).to_string()
        );
    }
}

#[test]
fn train_missing_target_edge_that_could_hide_nearer_base_stays_incomplete() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[("file", b"base")]);
    let middle = f.commit("main", Some(root.hash.clone()), &[("file", b"base")]);
    let a = f.commit("a", Some(root.hash), &[("file", b"source")]);
    let target = Commit::with_timestamp(
        "main".into(),
        Some(middle.hash),
        Some(Hash::from_hex(&"ab".repeat(32)).unwrap()),
        middle.manifest_hash,
        "qa".into(),
        None,
        vec![],
        chrono::Utc::now(),
    )
    .unwrap();
    f.repo.store_commit(&target).unwrap();
    f.repo.set_branch_head("main", &target.hash).unwrap();
    f.select(&a);
    let (output, json) = invoke(f.dir.path(), &["a"], false);
    assert!(!output.status.success());
    assert_eq!(json["state"], "incomplete");
    // Source-exclusive proof succeeds, but the independent exact resolver
    // rejects the missing target edge at depth 1 before the known base at 2.
    assert_eq!(json["steps"].as_array().unwrap().len(), 1);
    assert_eq!(json["reason"], "missing_merge_base");
    assert!(json["candidate_tree"].is_null());
}

#[test]
fn train_rejects_missing_exclusive_merge_parent_and_corrupt_older_target_history() {
    for corrupt_target in [false, true] {
        let f = Fixture::new();
        let older = f.commit("main", None, &[("file", b"base")]);
        let target = f.commit("main", Some(older.hash.clone()), &[("file", b"base")]);
        let mut a = f.commit("a", Some(target.hash), &[("file", b"source")]);
        if corrupt_target {
            let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
            conn.execute(
                "UPDATE commits SET parent_hash=? WHERE hash=?",
                [older.hash.as_str(), older.hash.as_str()],
            )
            .unwrap();
        } else {
            a = Commit::with_timestamp(
                "a".into(),
                a.parent_hash,
                Some(Hash::from_hex(&"cd".repeat(32)).unwrap()),
                a.manifest_hash,
                "qa".into(),
                None,
                vec![],
                chrono::Utc::now(),
            )
            .unwrap();
            f.repo.store_commit(&a).unwrap();
            f.repo.set_branch_head("a", &a.hash).unwrap();
        }
        f.select(&a);
        let (output, json) = invoke(f.dir.path(), &["a"], false);
        assert!(!output.status.success());
        assert_eq!(json["state"], "incomplete");
        assert_eq!(
            json["reason"],
            if corrupt_target {
                "corrupt_object"
            } else {
                "missing_ancestry"
            }
        );
        assert!(json["steps"].as_array().unwrap().is_empty());
        assert!(json["candidate_tree"].is_null());
    }
}

#[test]
fn train_incomplete_target_history_cannot_prove_declared_root_base() {
    let f = Fixture::new();
    let older = f.commit("main", None, &[("target", b"base")]);
    f.commit("main", Some(older.hash.clone()), &[("target", b"base")]);
    let a = f.commit("a", None, &[("source", b"root")]);
    f.select(&a);
    let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    conn.execute("DELETE FROM commits WHERE hash=?", [older.hash.as_str()])
        .unwrap();
    let (output, json) = invoke(f.dir.path(), &["a"], false);
    assert!(!output.status.success(), "{json}");
    assert_eq!(json["state"], "incomplete");
    assert_eq!(json["reason"], "missing_merge_base");
    assert!(json["candidate_tree"].is_null());
    assert_eq!(json["steps"][0]["fork_kind"], "unavailable");
}

#[test]
fn train_missing_base_commit_or_blob_never_becomes_empty_base() {
    for remove_commit in [false, true] {
        let f = Fixture::new();
        let base = f.commit("main", None, &[("file", b"base")]);
        f.commit("main", Some(base.hash.clone()), &[("file", b"target")]);
        let a = f.commit("a", Some(base.hash.clone()), &[("file", b"source")]);
        f.select(&a);
        let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        if remove_commit {
            conn.execute("DELETE FROM commits WHERE hash=?", [base.hash.as_str()])
                .unwrap();
        } else {
            conn.execute(
                "DELETE FROM blobs WHERE hash=?",
                [oak_core::hash_bytes(b"base").as_str()],
            )
            .unwrap();
        }
        let (output, json) = invoke(f.dir.path(), &["a"], false);
        assert!(!output.status.success());
        assert_eq!(json["state"], "incomplete");
        assert_eq!(
            json["reason"],
            if remove_commit {
                "missing_ancestry"
            } else {
                "missing_blob"
            }
        );
        assert!(json["candidate_tree"].is_null());
    }
}

#[test]
fn train_binary_overlap_is_explicit_and_never_uses_an_empty_text_base() {
    let f = Fixture::new();
    let root = f.commit("main", None, &[("file", &[255, 0, 12])]);
    let a = f.commit(
        "a",
        Some(root.hash.clone()),
        &[("file", b"same replacement")],
    );
    // Both branches independently replace binary base with different text.
    f.commit("b", Some(root.hash), &[("file", b"different replacement")]);
    f.select(&a);
    let (output, json) = invoke(f.dir.path(), &["a", "b"], false);
    assert!(!output.status.success());
    assert_eq!(json["state"], "conflict");
    assert_eq!(
        json["steps"][1]["conflict_files"][0]["conflict_type"],
        "binary_or_unresolved"
    );
}

#[test]
fn train_rejects_tampered_blob_and_declared_oversize_without_candidate() {
    for size in [6, 128 * 1024 * 1024 + 1] {
        let f = Fixture::new();
        let root = f.commit("main", None, &[("file", b"base")]);
        let a = f.commit("a", Some(root.hash), &[("file", b"source")]);
        f.select(&a);
        let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
        conn.execute(
            "UPDATE blobs SET content=?,size=?,codec=0 WHERE hash=?",
            rusqlite::params![
                b"forged".as_slice(),
                size,
                oak_core::hash_bytes(b"source").as_str()
            ],
        )
        .unwrap();
        let (output, json) = invoke(f.dir.path(), &["a"], false);
        assert!(!output.status.success());
        assert_eq!(json["state"], "incomplete");
        assert!(json["candidate_tree"].is_null());
        if size == 6 {
            assert_eq!(json["reason"], "corrupt_object");
        }
    }
}

#[tokio::test]
async fn train_remote_metadata_is_bounded_redacted_and_never_follows_redirects() {
    for response in [
        ResponseTemplate::new(500).set_body_string("metadata-private-secret"),
        ResponseTemplate::new(302)
            .insert_header("location", "https://private-secret@example.test/"),
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"branches":"metadata-private-secret"})),
        ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
    ] {
        let f = Fixture::new();
        let root = f.commit("main", None, &[("file", b"base")]);
        let a = f.commit("a", Some(root.hash), &[("file", b"source")]);
        f.select(&a);
        let server = MockServer::builder().start().await;
        f.repo
            .set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/api/oak/oak/branches"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let dir = f.dir.path().to_owned();
        let (output, json) = tokio::task::spawn_blocking(move || invoke(&dir, &["a"], true))
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(json.get("error").is_some());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-secret"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn train_remote_pins_heads_and_rejects_movement_without_changing_local_refs() {
    for moved in [false, true] {
        let f = Fixture::new();
        let older = f.commit("main", None, &[("file", b"base")]);
        let root = f.commit("main", Some(older.hash.clone()), &[("file", b"base")]);
        let remote_head = f.commit("a", Some(root.hash.clone()), &[("file", b"remote")]);
        let local_head = f.commit("a", Some(remote_head.hash.clone()), &[("file", b"local")]);
        f.select(&local_head);
        let conn = rusqlite::Connection::open(f.dir.path().join(".oak/oak.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        conn.execute("DELETE FROM commits WHERE hash=?", [older.hash.as_str()])
            .unwrap();
        let server = MockServer::builder().start().await;
        f.repo
            .set_metadata(MetadataKey::RemoteUrl, &server.uri())
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET")).and(path("/api/oak/oak/branches")).respond_with({
            let root = root.hash.clone(); let head = remote_head.hash.clone(); let changed = local_head.hash.clone(); let count = count.clone();
            move |_: &wiremock::Request| {
                let next = if count.fetch_add(1, Ordering::SeqCst) > 0 && moved { &changed } else { &head };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[{"name":"main","head":root},{"name":"a","head":next,"parent_branch":"main"}]}))
            }
        }).expect(2).mount(&server).await;
        let dir = f.dir.path().to_owned();
        let (output, json) = tokio::task::spawn_blocking(move || invoke(&dir, &["a"], true))
            .await
            .unwrap();
        assert_eq!(output.status.success(), !moved, "{json}");
        assert_eq!(
            json["ordered_sources"][0]["head"],
            remote_head.hash.to_string()
        );
        assert_eq!(json["state"], if moved { "stale" } else { "predicted" });
        assert_eq!(f.repo.get_branch_head("a").unwrap(), Some(local_head.hash));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
