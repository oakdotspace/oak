//! Local inspection and acquisition ergonomics (fb-460, fb-451, fb-519,
//! fb-522, fb-523, fb-68). Every command here is local and read-only against
//! the repository; `--output` / `export --tree-only` write only new paths.
use oak_core::{Branch, FileMode, Hash, ManifestEntry, Repository, SqliteRepository};
use std::path::Path;
use std::process::{Command, Output};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

struct Fixture {
    dir: tempfile::TempDir,
    head: Hash,
}

fn fixture(files: &[(&str, &[u8], FileMode)]) -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".oak")).unwrap();
    let repo = SqliteRepository::open(&dir.path().join(".oak/oak.db")).unwrap();
    let mut entries = Vec::new();
    for (path, content, mode) in files {
        entries.push(ManifestEntry {
            path: (*path).into(),
            blob_hash: repo.put_blob(content.to_vec()).unwrap(),
            mode: *mode,
        });
    }
    let tree = repo.put_manifest(entries).unwrap();
    let head = repo
        .put_commit(
            "main".into(),
            None,
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.set_head(&head).unwrap();
    Fixture { dir, head }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------- fb-460

#[test]
fn refs_inspect_reports_one_consistent_snapshot() {
    let fx = fixture(&[("a", b"a", FileMode::Regular)]);
    let repo = SqliteRepository::open(&fx.dir.path().join(".oak/oak.db")).unwrap();
    repo.set_branch_head("main", &fx.head).unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_current_branch("topic").unwrap();
    drop(repo);

    let out = oak(fx.dir.path(), &["refs", "inspect", "--json"]);
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    let refs = &value["refs"];
    assert_eq!(value["consistent"], true);
    assert_eq!(refs["snapshot"]["single_read_transaction"], true);
    assert_eq!(refs["current_branch"], "topic");
    assert_eq!(refs["effective_head"]["commit"], fx.head.as_str());
    // topic has no head of its own: it inherits main's through parent_branch.
    assert_eq!(refs["effective_head"]["source"], "parent_inheritance");
    assert_eq!(
        refs["effective_head"]["branch_chain"],
        serde_json::json!(["topic", "main"])
    );
    let topic = refs["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "topic")
        .unwrap();
    assert_eq!(topic["inherited_from"], "main");
    assert_eq!(topic["current"], true);
    assert!(topic.get("head").is_none());
}

#[test]
fn refs_inspect_flags_a_divergent_legacy_head() {
    let fx = fixture(&[("a", b"a", FileMode::Regular)]);
    let repo = SqliteRepository::open(&fx.dir.path().join(".oak/oak.db")).unwrap();
    let blob = repo.put_blob(b"other".to_vec()).unwrap();
    let tree = repo
        .put_manifest(vec![ManifestEntry {
            path: "a".into(),
            blob_hash: blob,
            mode: FileMode::Regular,
        }])
        .unwrap();
    let other = repo
        .put_commit(
            "main".into(),
            Some(fx.head.clone()),
            None,
            tree,
            "tester".into(),
            None,
            chrono::Utc::now(),
            vec![],
        )
        .unwrap();
    repo.store_branch(&Branch::new("topic".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("topic", &fx.head).unwrap();
    repo.set_current_branch("topic").unwrap();
    // Inject a divergent legacy pointer.
    repo.set_head(&other).unwrap();
    drop(repo);

    let out = oak(fx.dir.path(), &["refs", "inspect", "--json"]);
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["consistent"], false);
    let refs = &value["refs"];
    assert_eq!(refs["legacy_head"], other.as_str());
    // The branch head wins while attached, exactly as `oak hash` resolves it.
    assert_eq!(refs["effective_head"]["commit"], fx.head.as_str());
    assert_eq!(refs["effective_head"]["source"], "current_branch_head");
    let hash = oak(fx.dir.path(), &["hash"]);
    assert_eq!(
        String::from_utf8_lossy(&hash.stdout).trim(),
        fx.head.as_str()
    );
    let kinds: Vec<_> = refs["disagreements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, vec!["legacy_head_differs"]);
}

#[test]
fn refs_inspect_detached_cycles_and_missing_commits() {
    // Detached: the legacy pointer is authoritative.
    let fx = fixture(&[("a", b"a", FileMode::Regular)]);
    let out = oak(fx.dir.path(), &["refs", "inspect", "--json"]);
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    assert_eq!(value["refs"]["attached"], false);
    assert_eq!(
        value["refs"]["effective_head"]["source"],
        "detached_legacy_head"
    );

    // Parent cycle and a dangling head row.
    let conn = rusqlite::Connection::open(fx.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         INSERT INTO branches(name,parent_branch) VALUES('x','y'),('y','x');
         INSERT INTO metadata(key,value) VALUES('current_branch','x');
         INSERT INTO branch_heads(branch_name,head_hash) VALUES('ghost',
           'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');",
    )
    .unwrap();
    drop(conn);
    let out = oak(fx.dir.path(), &["refs", "inspect", "--json"]);
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["refs"]["effective_head"]["status"], "corrupt");
    let kinds: Vec<_> = value["refs"]["disagreements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"parent_cycle".to_string()), "{kinds:?}");
    assert!(
        kinds.contains(&"head_commit_missing".to_string()),
        "{kinds:?}"
    );

    let out = oak(
        fx.dir.path(),
        &["refs", "inspect", "--json", "--max-branches", "1"],
    );
    let value = json(&out);
    assert_eq!(value["refs"]["branches_truncated"], true);
    assert_eq!(value["refs"]["branches"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------- fb-451

#[test]
fn file_inspect_output_writes_verified_logical_bytes_once() {
    let big = vec![b'z'; 200_000];
    let fx = fixture(&[("dir/big", &big, FileMode::Executable)]);
    let out_dir = tempfile::TempDir::new().unwrap();
    let target = out_dir.path().join("big.out");
    let target_arg = target.to_str().unwrap();
    let out = oak(
        fx.dir.path(),
        &[
            "file", "inspect", "--at", "HEAD", "dir/big", "--output", target_arg, "--json",
        ],
    );
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    assert_eq!(std::fs::read(&target).unwrap(), big);
    assert_eq!(value["output"]["written"], true);
    assert_eq!(value["output"]["bytes"], 200_000);
    let evidence = &value["evidence"];
    assert_eq!(evidence["verified_size"], 200_000);
    assert_eq!(evidence["content_sha256"], sha256(&big));
    // Logical identity is separate from the (compressed) stored representation.
    assert_eq!(evidence["storage"]["codec"], "zstd");
    assert!(evidence["storage"]["stored_bytes"].as_u64().unwrap() < 200_000);
    // No temp file is left beside the output.
    assert_eq!(std::fs::read_dir(out_dir.path()).unwrap().count(), 1);

    // Never overwrite.
    std::fs::write(&target, b"keep me").unwrap();
    let out = oak(
        fx.dir.path(),
        &[
            "file", "inspect", "--at", "HEAD", "dir/big", "--output", target_arg, "--json",
        ],
    );
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&target).unwrap(), b"keep me");

    // `..` resolves lexically against cwd: a relative path outside the
    // repository works (QA L4) ...
    std::fs::create_dir(fx.dir.path().join("sub")).unwrap();
    let outside = fx.dir.path().join("../w3c-lexical-out");
    let _ = std::fs::remove_file(&outside);
    let out = oak(
        &fx.dir.path().join("sub"),
        &[
            "file",
            "inspect",
            "--at",
            "HEAD",
            "dir/big",
            "--output",
            "../../w3c-lexical-out",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(std::fs::read(&outside).unwrap(), big);
    std::fs::remove_file(&outside).unwrap();
    // ... while any spelling that lands inside .oak is still refused.
    for bad in [
        ".oak/stolen".to_string(),
        "sub/../.oak/stolen".to_string(),
        fx.dir.path().join(".oak/stolen").display().to_string(),
    ] {
        let out = oak(
            fx.dir.path(),
            &[
                "file", "inspect", "--at", "HEAD", "dir/big", "--output", &bad, "--json",
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
    assert!(!fx.dir.path().join(".oak/stolen").exists());
}

#[test]
fn file_inspect_output_is_never_created_for_unverified_content() {
    let fx = fixture(&[("f", b"payload bytes", FileMode::Regular)]);
    let conn = rusqlite::Connection::open(fx.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "UPDATE blobs SET content=?1,codec=0 WHERE size=13",
        [b"tampered byte".as_slice()],
    )
    .unwrap();
    drop(conn);
    let out_dir = tempfile::TempDir::new().unwrap();
    let target = out_dir.path().join("f.out");
    let out = oak(
        fx.dir.path(),
        &[
            "file",
            "inspect",
            "--at",
            "HEAD",
            "f",
            "--output",
            target.to_str().unwrap(),
            "--json",
        ],
    );
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["evidence"]["status"], "corrupt");
    assert_eq!(value["output"]["written"], false);
    assert!(value["evidence"].get("content_sha256").is_none());
    assert_eq!(std::fs::read_dir(out_dir.path()).unwrap().count(), 0);
}

#[test]
fn file_inspect_empty_blob_reports_implicit_storage() {
    let fx = fixture(&[("empty", b"", FileMode::Regular)]);
    let conn = rusqlite::Connection::open(fx.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute("DELETE FROM blobs WHERE size=0", []).unwrap();
    drop(conn);
    let out = oak(
        fx.dir.path(),
        &["file", "inspect", "--at", "HEAD", "empty", "--json"],
    );
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    assert_eq!(
        value["evidence"]["storage"]["representation"],
        "implicit_empty"
    );
    assert_eq!(value["evidence"]["content_sha256"], sha256(b""));
}

// ---------------------------------------------------------------- fb-519

#[test]
fn tree_inspect_lists_every_path_with_verified_digests() {
    let files: &[(&str, &[u8], FileMode)] = &[
        ("README", b"hello\n", FileMode::Regular),
        ("bin/run", b"#!/bin/sh\n", FileMode::Executable),
        ("link", b"README", FileMode::Symlink),
        ("src/a/deep.rs", b"fn main() {}\n", FileMode::Regular),
        ("src/dup.rs", b"hello\n", FileMode::Regular),
    ];
    let fx = fixture(files);
    let out = oak(
        fx.dir.path(),
        &["tree", "inspect", "--at", fx.head.as_str(), "--json"],
    );
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    let evidence = &value["evidence"];
    assert_eq!(evidence["complete"], true);
    assert_eq!(evidence["truncated"], false);
    assert_eq!(evidence["commit"], fx.head.as_str());
    assert_eq!(evidence["file_count"], 5);
    let listed: Vec<_> = evidence["files"].as_array().unwrap().iter().collect();
    assert_eq!(listed.len(), 5);
    for (path, content, mode) in files {
        let entry = listed.iter().find(|f| f["path"] == *path).unwrap();
        assert_eq!(entry["size"], content.len());
        assert_eq!(entry["sha256"], sha256(content));
        assert_eq!(entry["mode"], serde_json::to_value(mode).unwrap());
        assert_eq!(entry["status"], "verified");
    }
}

#[test]
fn tree_inspect_truncates_explicitly_and_flags_bad_objects() {
    let fx = fixture(&[
        ("a", b"1111", FileMode::Regular),
        ("b", b"2222", FileMode::Regular),
        ("c", b"3333", FileMode::Regular),
    ]);
    let at = fx.head.to_string();
    let out = oak(
        fx.dir.path(),
        &["tree", "inspect", "--at", &at, "--json", "--max-files", "2"],
    );
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    let evidence = &value["evidence"];
    assert_eq!(evidence["complete"], false);
    assert_eq!(evidence["truncation"]["limit"], "max_files");
    assert_eq!(evidence["truncation"]["next_path"], "c");
    assert_eq!(evidence["files"].as_array().unwrap().len(), 2);
    assert!(value["recommended_next_commands"][0]
        .as_str()
        .unwrap()
        .contains("--max-files 4"));

    let out = oak(
        fx.dir.path(),
        &["tree", "inspect", "--at", &at, "--json", "--max-bytes", "5"],
    );
    let value = json(&out);
    assert_eq!(value["evidence"]["truncation"]["limit"], "max_bytes");
    assert_eq!(value["evidence"]["truncation"]["next_path"], "b");
    assert_eq!(value["evidence"]["logical_bytes"], 4);

    // A missing blob stays listed, with its own status; the listing is incomplete.
    let conn = rusqlite::Connection::open(fx.dir.path().join(".oak/oak.db")).unwrap();
    let deleted = conn
        .execute("DELETE FROM blobs WHERE content=?1", [b"2222".as_slice()])
        .unwrap();
    assert_eq!(deleted, 1);
    drop(conn);
    let out = oak(fx.dir.path(), &["tree", "inspect", "--at", &at, "--json"]);
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["evidence"]["status"], "object_missing");
    assert_eq!(value["evidence"]["files"].as_array().unwrap().len(), 3);
    assert_eq!(value["evidence"]["complete"], false);

    // Abbreviated hashes are refused (the pin must be exact).
    let out = oak(
        fx.dir.path(),
        &["tree", "inspect", "--at", &at[..12], "--json"],
    );
    assert_eq!(out.status.code(), Some(2));
}

// ---------------------------------------------------------------- fb-522

#[test]
fn export_tree_only_materializes_one_verified_tree() {
    let fx = fixture(&[
        ("README", b"hello\n", FileMode::Regular),
        ("bin/run", b"#!/bin/sh\n", FileMode::Executable),
        ("link", b"README", FileMode::Symlink),
        ("src/a/deep.rs", b"fn main() {}\n", FileMode::Regular),
    ]);
    let parent = tempfile::TempDir::new().unwrap();
    let dest = parent.path().join("out");
    let dest_arg = dest.to_str().unwrap();
    let at = fx.head.to_string();
    let out = oak(
        fx.dir.path(),
        &["export", "--tree-only", "--at", &at, dest_arg, "--json"],
    );
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    assert_eq!(value["materialized"], true);
    assert_eq!(value["history_replayed"], false);
    assert_eq!(value["commit"], at.as_str());
    assert_eq!(std::fs::read(dest.join("README")).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read(dest.join("src/a/deep.rs")).unwrap(),
        b"fn main() {}\n"
    );
    assert!(!dest.join(".oak").exists());
    assert!(!dest.join(".git").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dest.join("bin/run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            std::path::PathBuf::from("README")
        );
    }
    // Only DEST was created in the parent (staging removed by rename).
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);

    // A non-empty DEST is refused and left untouched.
    let out = oak(
        fx.dir.path(),
        &["export", "--tree-only", "--at", &at, dest_arg, "--json"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(dest.join("README")).unwrap(), b"hello\n");

    // An empty existing directory is accepted.
    let empty = parent.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let out = oak(
        fx.dir.path(),
        &[
            "export",
            "--tree-only",
            "--at",
            &at,
            empty.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(empty.join("README").exists());

    // --tree-only requires --at; --at requires --tree-only.
    assert_eq!(
        oak(fx.dir.path(), &["export", "--tree-only", "x"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        oak(fx.dir.path(), &["export", "--at", &at, "x"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn export_tree_only_streams_large_files_and_sweeps_dead_staging() {
    // Larger than the 256 KiB write buffer and several zstd frames: the
    // file is streamed through the sink, never held whole (QA M2).
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let fx = fixture(&[("big.bin", &big, FileMode::Regular)]);
    let parent = tempfile::TempDir::new().unwrap();
    // Leftovers: one from a dead process (removed), one from a live process
    // (this test; kept), one unrelated name (kept).
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let dead_dir = parent.path().join(format!(".out.oak-export-{dead}-abc"));
    let live_dir = parent
        .path()
        .join(format!(".out.oak-export-{}-abc", std::process::id()));
    let other = parent.path().join(".other.oak-export-1-abc");
    for dir in [&dead_dir, &live_dir, &other] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("partial"), b"x").unwrap();
    }
    // Relative DEST with `..` resolves lexically against cwd (QA L4).
    std::fs::create_dir(fx.dir.path().join("sub")).unwrap();
    let rel = format!("{}/out", parent.path().display());
    let out = oak(
        &fx.dir.path().join("sub"),
        &["export", "--tree-only", "--at", "HEAD", &rel, "--json"],
    );
    let value = json(&out);
    assert!(out.status.success(), "{value}");
    assert_eq!(value["stale_staging_removed"], 1);
    assert!(!dead_dir.exists());
    assert!(live_dir.exists() && other.exists());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(value["durability"], "synced_before_publish");
    assert_eq!(
        std::fs::read(parent.path().join("out/big.bin")).unwrap(),
        big
    );

    let nested = tempfile::TempDir::new_in(fx.dir.path()).unwrap();
    let out = oak(
        nested.path(),
        &[
            "export",
            "--tree-only",
            "--at",
            "HEAD",
            "../.oak/x",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(!fx.dir.path().join(".oak/x").exists());
}

#[test]
fn export_tree_only_publishes_nothing_on_budget_or_corruption() {
    let fx = fixture(&[
        ("a", b"aaaa", FileMode::Regular),
        ("b", b"bbbb", FileMode::Regular),
    ]);
    let parent = tempfile::TempDir::new().unwrap();
    let dest = parent.path().join("out");
    let at = fx.head.to_string();
    let out = oak(
        fx.dir.path(),
        &[
            "export",
            "--tree-only",
            "--at",
            &at,
            dest.to_str().unwrap(),
            "--json",
            "--max-files",
            "1",
        ],
    );
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["materialized"], false);
    assert_eq!(value["truncation"]["limit"], "max_files");
    assert!(!dest.exists());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);

    let conn = rusqlite::Connection::open(fx.dir.path().join(".oak/oak.db")).unwrap();
    conn.execute(
        "UPDATE blobs SET content=?1,codec=0 WHERE rowid=(SELECT max(rowid) FROM blobs)",
        [b"XXXX".as_slice()],
    )
    .unwrap();
    drop(conn);
    let out = oak(
        fx.dir.path(),
        &[
            "export",
            "--tree-only",
            "--at",
            &at,
            dest.to_str().unwrap(),
            "--json",
        ],
    );
    let value = json(&out);
    assert_eq!(out.status.code(), Some(1), "{value}");
    assert_eq!(value["status"], "corrupt");
    assert!(!dest.exists());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

// ---------------------------------------------------------------- fb-523

#[test]
fn environment_reports_presence_of_secrets_never_values() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .env("OAK_API_KEY", "super-secret-token-value")
        .env("OAK_REMOTE", "https://user:pw@example.test/")
        .env("OAK_PROBE_TIMEOUT_SECS", "7")
        .env_remove("OAK_NO_UPDATE_CHECK")
        .args(["environment", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("super-secret-token-value"));
    assert!(!text.contains("pw@"));
    let value = json(&out);
    let var = |name: &str| {
        value["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    };
    assert_eq!(var("OAK_API_KEY")["set"], true);
    assert!(var("OAK_API_KEY").get("value").is_none());
    assert_eq!(var("OAK_REMOTE")["value"], "https://example.test");
    assert_eq!(var("OAK_PROBE_TIMEOUT_SECS")["value"], "7");
    assert_eq!(var("OAK_DIFF_TOOL")["set"], false);
    for name in [
        "OAK_NO_UPDATE_CHECK",
        "OAK_REPO",
        "OAK_ALLOW_PARTIAL_CLONE",
        "OAK_VERBOSE",
    ] {
        var(name);
    }
    let check = &value["background_network"]["update_check"];
    assert_eq!(check["enabled"], true);
    assert_eq!(check["interval_secs"], 86_400);
    assert_eq!(check["timeout_secs"], 2);

    let out = oak(dir.path(), &["env", "--json"]);
    let value = json(&out);
    assert_eq!(
        value["background_network"]["update_check"]["enabled"],
        false
    );
    assert_eq!(
        value["background_network"]["update_check"]["disabled_by"],
        "OAK_NO_UPDATE_CHECK"
    );
}

// ---------------------------------------------------------------- fb-68

#[test]
fn commit_on_a_detached_head_names_switch_c() {
    let fx = fixture(&[("a", b"a", FileMode::Regular)]);
    std::fs::write(fx.dir.path().join("a"), b"changed").unwrap();
    let out = oak(fx.dir.path(), &["commit", "--json"]);
    assert!(!out.status.success());
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("HEAD is detached"), "{text}");
    assert!(text.contains("oak switch -c"), "{text}");
    let value = json(&out);
    assert_eq!(value["error"]["code"], "invalid_argument");
    assert_eq!(
        value["error"]["recommended_next_commands"],
        serde_json::json!(["oak switch -c <name>"])
    );

    // `oak status` no longer stays silent on a detached HEAD (QA L5): stdout
    // stays porcelain, the note goes to stderr.
    let out = oak(fx.dir.path(), &["status"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HEAD is detached at"), "{stderr}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("detached"));
}

#[test]
fn environment_redacts_proxy_and_trusted_remote_credentials() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .env("HTTPS_PROXY", "http://user:SEKRET1@proxy.test:3128")
        .env("all_proxy", "socks5://u:SEKRET2@socks.test:1080")
        .env("NO_PROXY", "localhost,.internal")
        .env(
            "OAK_TRUSTED_REMOTES",
            "https://a:SEKRET3@one.test, https://two.test/?k=SEKRET4",
        )
        .args(["environment", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("SEKRET"), "{text}");
    let value = json(&out);
    assert_eq!(value["coverage"], "oak_reads_plus_known_dependency_vars");
    let var = |name: &str| {
        value["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    };
    assert_eq!(var("HTTPS_PROXY")["value"], "http://proxy.test:3128");
    assert_eq!(var("HTTPS_PROXY")["read_by"], "dependency");
    assert_eq!(var("all_proxy")["value"], "socks5://socks.test:1080");
    assert_eq!(var("NO_PROXY")["value"], "localhost,.internal");
    assert_eq!(
        var("OAK_TRUSTED_REMOTES")["value"],
        "https://one.test,https://two.test"
    );
    assert_eq!(var("OAK_REMOTE")["read_by"], "oak");
}

#[test]
fn serve_help_never_prints_the_token_value() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir.path())
        .env("OAK_SERVE_TOKEN", "supersecret123")
        .args(["serve", "--help"])
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("OAK_SERVE_TOKEN"), "{text}");
    assert!(!text.contains("supersecret123"), "{text}");
}

#[test]
fn clone_detached_rejects_branch_selection_before_network() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = oak(
        dir.path(),
        &[
            "clone",
            "o/r",
            "d",
            "--detached",
            "--branch",
            "x",
            "--remote",
            "http://127.0.0.1:9",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.path().join("d").exists());
}
