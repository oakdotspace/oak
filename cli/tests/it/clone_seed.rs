//! fb-448 / fb-335: `oak clone ORG/REPO DEST --from LOCAL_CHECKOUT` against a
//! real loopback `oak serve`. The seed may only contribute hash-verified
//! content chunks; the result must match a plain clone.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use oak_core::{MetadataKey, Repository, SqliteRepository};
use serde_json::Value;

const TOKEN: &str = "clone-seed-token";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn oak(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", TOKEN)
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .args(args)
        .output()
        .unwrap()
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

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON document ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
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
            TOKEN,
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

/// Deterministic pseudo-random bytes so chunks are not trivially compressible.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect()
}

struct Fixture {
    _temp: tempfile::TempDir,
    _serve: ChildGuard,
    base: String,
    home: std::path::PathBuf,
    root: std::path::PathBuf,
}

/// Publish checkout `a` (two files, one multi-megabyte) to `oak/<repo>`.
async fn published(repo: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let a = root.join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (serve, base) = start_serve(&root.join("serve-data")).await;
    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("big.bin"), noise(3_000_000, 7)).unwrap();
    std::fs::write(a.join("small.txt"), "small\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit");
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
    Fixture {
        _temp: temp,
        _serve: serve,
        base,
        home,
        root,
    }
}

fn clone_json(fx: &Fixture, repo: &str, dest: &str, from: Option<&str>) -> Value {
    let spec = format!("oak/{repo}");
    let mut args = vec![
        "clone",
        spec.as_str(),
        dest,
        "-r",
        fx.base.as_str(),
        "--json",
    ];
    if let Some(from) = from {
        args.extend(["--from", from]);
    }
    json(&ok(oak(&fx.home, &fx.root, &args), "clone"))
}

fn tree_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name() != ".oak")
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

fn db_bytes(dir: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(dir.join(".oak/oak.db")).unwrap();
    if let Ok(wal) = std::fs::read(dir.join(".oak/oak.db-wal")) {
        bytes.extend(wal);
    }
    bytes
}

#[tokio::test(flavor = "current_thread")]
async fn clone_from_local_checkout_reuses_verified_chunks_and_matches_plain_clone() {
    let fx = published("seeded").await;
    let a = fx.root.join("a");
    // Source-only state that must never cross into the clone.
    {
        let repo = SqliteRepository::open(&a.join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::ApiKey, "SOURCE-ONLY-SECRET-KEY")
            .unwrap();
        repo.set_metadata(MetadataKey::SparsePaths, "source-only-cone")
            .unwrap();
    }
    std::fs::write(a.join("dirty-untracked.txt"), "dirty\n").unwrap();
    std::fs::write(a.join("small.txt"), "dirty edit\n").unwrap();

    let plain = clone_json(&fx, "seeded", "plain", None);
    assert!(
        plain["observed"]["chunks"]["downloaded_unique"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(plain.get("seed").is_none());

    // Seed from the pusher (whole blobs only) ...
    let seeded = clone_json(&fx, "seeded", "b", Some("a"));
    let required = seeded["observed"]["chunks"]["required_unique"]
        .as_u64()
        .unwrap();
    assert!(required > 0);
    assert_eq!(seeded["observed"]["chunks"]["downloaded_unique"], 0);
    // The destination was empty at clone start; seeded chunks are separate.
    assert_eq!(
        seeded["observed"]["chunks"]["present_at_clone_start_unique"],
        0
    );
    assert_eq!(
        seeded["observed"]["chunks"]["seeded_from_local_unique"],
        required
    );
    assert_eq!(seeded["seed"]["identity_verified"], true);
    assert_eq!(seeded["seed"]["reuse_scope"], "content_chunks_only");
    assert_eq!(
        seeded["seed"]["chunks_reused_from_source_blobs"]
            .as_u64()
            .unwrap()
            + seeded["seed"]["chunks_reused_from_source_chunks"]
                .as_u64()
                .unwrap(),
        required
    );
    // ... and from a clone (chunk store).
    let from_clone = clone_json(&fx, "seeded", "c", Some("plain"));
    assert_eq!(from_clone["observed"]["chunks"]["downloaded_unique"], 0);
    assert_eq!(
        from_clone["seed"]["chunks_reused_from_source_chunks"],
        required
    );

    // Same server head/manifest as the plain clone; committed content only.
    for receipt in [&seeded, &from_clone] {
        assert_eq!(receipt["result"]["head"], plain["result"]["head"]);
        assert_eq!(receipt["result"]["manifest"], plain["result"]["manifest"]);
    }
    let b = fx.root.join("b");
    assert_eq!(tree_files(&b), tree_files(&fx.root.join("plain")));
    assert!(!b.join("dirty-untracked.txt").exists());
    assert_eq!(
        std::fs::read_to_string(b.join("small.txt")).unwrap(),
        "small\n"
    );

    // `oak status` clean, exactly like a normal clone.
    let status = json(&ok(oak(&fx.home, &b, &["status", "--json"]), "status"));
    assert_eq!(status["changes"].as_array().unwrap().len(), 0);
    assert_eq!(status["head"], plain["result"]["head"]);

    // No credentials or metadata from the source.
    let dest = SqliteRepository::open(&b.join(".oak/oak.db")).unwrap();
    assert_ne!(
        dest.get_metadata(MetadataKey::ApiKey).unwrap().as_deref(),
        Some("SOURCE-ONLY-SECRET-KEY")
    );
    assert_eq!(dest.get_metadata(MetadataKey::SparsePaths).unwrap(), None);
    drop(dest);
    let bytes = db_bytes(&b);
    assert!(!bytes
        .windows(b"SOURCE-ONLY-SECRET-KEY".len())
        .any(|w| w == b"SOURCE-ONLY-SECRET-KEY"));
    assert!(!b.join(".oak/wdlock").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn clone_from_rejects_corrupt_source_chunks_and_downloads_them_instead() {
    let fx = published("corrupt").await;
    clone_json(&fx, "corrupt", "plain", None);
    let plain = fx.root.join("plain");
    {
        let conn = rusqlite::Connection::open(plain.join(".oak/oak.db")).unwrap();
        // One chunk with wrong bytes, one with a malformed (TEXT) row.
        let changed = conn
            .execute(
                "UPDATE chunks SET content = zeroblob(length(content)) WHERE rowid = (SELECT MIN(rowid) FROM chunks)",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
        let changed = conn
            .execute(
                "UPDATE chunks SET content = 'not bytes' WHERE rowid = (SELECT MAX(rowid) FROM chunks)",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }
    // Bad chunk rows are rejected; the verified whole blobs still cover them.
    let seeded = clone_json(&fx, "corrupt", "b", Some("plain"));
    // Counted once each: recovered chunks are reused, not also rejected.
    assert_eq!(seeded["seed"]["chunks_rejected_hash_mismatch"], 0);
    assert_eq!(seeded["seed"]["source_read_errors"], 1);
    assert_eq!(seeded["seed"]["chunks_reused_from_source_blobs"], 2);
    assert_eq!(seeded["observed"]["chunks"]["downloaded_unique"], 0);

    // With the whole blobs damaged too, nothing is reusable: every chunk is
    // downloaded and verified from the server instead.
    {
        let conn = rusqlite::Connection::open(plain.join(".oak/oak.db")).unwrap();
        conn.execute("UPDATE blobs SET content = zeroblob(length(content))", [])
            .unwrap();
    }
    let fallback = clone_json(&fx, "corrupt", "b2", Some("plain"));
    assert_eq!(fallback["seed"]["chunks_reused_from_source_blobs"], 0);
    assert_eq!(fallback["seed"]["chunks_reused_from_source_chunks"], 0);
    // The source held bytes for both chunks, none usable: rejected, not absent.
    assert_eq!(fallback["seed"]["chunks_rejected_hash_mismatch"], 2);
    assert_eq!(fallback["seed"]["chunks_absent_in_source"], 0);
    assert_eq!(fallback["observed"]["chunks"]["downloaded_unique"], 2);

    for dest in ["b", "b2"] {
        let dir = fx.root.join(dest);
        assert_eq!(tree_files(&dir), tree_files(&fx.root.join("a")));
        let status = json(&ok(oak(&fx.home, &dir, &["status", "--json"]), "status"));
        assert_eq!(status["changes"].as_array().unwrap().len(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clone_from_refuses_identity_mismatch_before_creating_anything() {
    let fx = published("bound").await;
    // A checkout of a different repository.
    let other = fx.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    ok(oak(&fx.home, &other, &["init", "."]), "init other");
    std::fs::write(other.join("x.txt"), "x\n").unwrap();
    ok(oak(&fx.home, &other, &["commit"]), "commit other");
    ok(
        oak(
            &fx.home,
            &other,
            &["push", "--repo", "oak/unrelated", "-r", &fx.base, "--json"],
        ),
        "push other",
    );
    // A same-named repository reached through a different remote origin.
    let foreign = fx.root.join("foreign");
    std::fs::create_dir_all(foreign.join(".oak")).unwrap();
    let repo = SqliteRepository::open(&foreign.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, "https://elsewhere.example")
        .unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "bound").unwrap();
    drop(repo);

    for (source, dest) in [
        ("other", "d1"),
        ("foreign", "d2"),
        ("missing", "d3"),
        ("a/..", "d4"),
    ] {
        let out = oak(
            &fx.home,
            &fx.root,
            &[
                "clone",
                "oak/bound",
                dest,
                "-r",
                &fx.base,
                "--json",
                "--from",
                source,
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{source}: {:?}", out);
        let value = json(&out);
        assert_eq!(value["error"]["code"], "invalid_argument", "{value}");
        assert!(!fx.root.join(dest).exists(), "{source} created {dest}");
    }
    // The destination may not overlap the source.
    let out = oak(
        &fx.home,
        &fx.root,
        &[
            "clone",
            "oak/bound",
            "a/nested",
            "-r",
            &fx.base,
            "--from",
            "a",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(!fx.root.join("a/nested").exists());
}
