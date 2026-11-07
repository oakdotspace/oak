//! `oak release` / `oak tag` end to end against a real loopback `oak serve`:
//! tags on main, assets through the chunk store, the pull/clone tag cache
//! (metadata synced, asset bytes never downloaded until asked), verified
//! downloads, and deletion propagating on pull.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use oak_core::{Hash, Repository, SqliteRepository};
use serde_json::Value;

const SERVE_TOKEN: &str = "release-serve-token";

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
        .env("OAK_API_KEY", SERVE_TOKEN)
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

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Deterministic bytes that FastCDC splits into several chunks.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releases_round_trip_through_serve_pull_and_clone() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let a = root.join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (_serve, base) = start_serve(&root.join("serve-data")).await;

    // A repo with one commit on main.
    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("README.md"), "hello\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit");
    ok(oak(&home, &a, &["desc", "first"]), "desc");
    ok(
        oak(
            &home,
            &a,
            &["push", "--repo", "oak/rel", "-r", &base, "--json"],
        ),
        "push",
    );
    ok(oak(&home, &a, &["merge", "--json"]), "merge");

    // Capabilities advertise the protocol.
    let caps: Value = reqwest::Client::new()
        .get(format!("{base}/api/capabilities"))
        .bearer_auth(SERVE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(caps["release_protocol"], "v1");

    // Create a release with a multi-chunk asset (above the 10 MB threshold)
    // and a small one.
    let big = noise(12 * 1024 * 1024 + 4321, 3);
    std::fs::write(root.join("oak.tar.gz"), &big).unwrap();
    std::fs::write(root.join("SHA256SUMS.txt"), "sums\n").unwrap();
    let created = json(&ok(
        oak(
            &home,
            &a,
            &[
                "release",
                "create",
                "v1.0.0",
                root.join("oak.tar.gz").to_str().unwrap(),
                root.join("SHA256SUMS.txt").to_str().unwrap(),
                "--title",
                "First",
                "--notes",
                "## Changes\n- everything",
                "--json",
            ],
        ),
        "release create",
    ));
    assert_eq!(created["outcome"], "created", "{created}");
    let uploads = created["uploads"].as_array().unwrap();
    assert_eq!(uploads.len(), 2);
    let tarball = uploads.iter().find(|u| u["name"] == "oak.tar.gz").unwrap();
    assert!(tarball["chunks"].as_u64().unwrap() > 1, "{tarball}");
    assert_eq!(
        tarball["blob_hash"].as_str().unwrap(),
        oak_core::hash_bytes(&big).as_str()
    );
    assert_eq!(created["tag"]["assets"].as_array().unwrap().len(), 2);

    // `oak tag` is the same command, and a tag with no details lists like any
    // other release.
    let bare = json(&ok(
        oak(&home, &a, &["tag", "create", "checkpoint", "--json"]),
        "tag create",
    ));
    assert!(bare["tag"]["title"].is_null(), "{bare}");
    assert!(bare["tag"].get("is_release").is_none(), "{bare}");
    let releases = json(&ok(
        oak(&home, &a, &["release", "list", "--json"]),
        "list releases",
    ));
    let mut names: Vec<&str> = releases["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["checkpoint", "v1.0.0"]);

    // Creating the same tag again is idempotent; a bad name is a usage error.
    let again = oak(&home, &a, &["release", "create", "checkpoint", "--json"]);
    assert_eq!(json(&ok(again, "recreate"))["outcome"], "unchanged");
    let bad = oak(&home, &a, &["release", "create", "-bad", "--json"]);
    assert_eq!(bad.status.code(), Some(2));

    // A clone gets the tag metadata but none of the asset bytes.
    let b = root.join("b");
    ok(
        oak(
            &home,
            &root,
            &["clone", "oak/rel", "b", "-r", &base, "--json"],
        ),
        "clone",
    );
    let offline = json(&ok(
        oak(
            &home,
            &b,
            &["release", "view", "v1.0.0", "--offline", "--json"],
        ),
        "offline view",
    ));
    assert_eq!(offline["source"], "local");
    assert_eq!(offline["tag"]["title"], "First");
    assert_eq!(offline["tag"]["assets"].as_array().unwrap().len(), 2);
    {
        let repo = SqliteRepository::open(&b.join(".oak/oak.db")).unwrap();
        let hash = Hash::from_hex(tarball["blob_hash"].as_str().unwrap()).unwrap();
        assert!(repo.get_blob_chunks(&hash).unwrap().is_none());
        assert!(repo.get_blob(&hash).unwrap().is_none());
    }

    // Download on demand, verified; a second download is a no-op.
    let out = root.join("downloads");
    let downloaded = json(&ok(
        oak(
            &home,
            &b,
            &[
                "release",
                "download",
                "v1.0.0",
                "oak.tar.gz",
                "-d",
                out.to_str().unwrap(),
                "--json",
            ],
        ),
        "download",
    ));
    assert_eq!(downloaded["downloads"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(out.join("oak.tar.gz")).unwrap(), big);
    let again = json(&ok(
        oak(
            &home,
            &b,
            &[
                "release",
                "download",
                "v1.0.0",
                "-d",
                out.to_str().unwrap(),
                "--json",
            ],
        ),
        "download again",
    ));
    let rows = again["downloads"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["already_present"] == true));

    // Finalize freezes assets.
    ok(
        oak(
            &home,
            &a,
            &["release", "edit", "v1.0.0", "--finalize", "--json"],
        ),
        "finalize",
    );
    let frozen = oak(
        &home,
        &a,
        &[
            "release",
            "upload",
            "v1.0.0",
            root.join("SHA256SUMS.txt").to_str().unwrap(),
            "--as",
            "other.txt",
            "--json",
        ],
    );
    assert!(!frozen.status.success());

    // Deleting needs --yes, and propagates to other checkouts on pull.
    assert_eq!(
        oak(&home, &a, &["release", "delete", "v1.0.0"])
            .status
            .code(),
        Some(2)
    );
    ok(
        oak(
            &home,
            &a,
            &["release", "delete", "v1.0.0", "--yes", "--json"],
        ),
        "delete",
    );
    ok(oak(&home, &b, &["pull", "--json"]), "pull");
    let after = json(&ok(
        oak(&home, &b, &["release", "list", "--offline", "--json"]),
        "offline list",
    ));
    let names: Vec<&str> = after["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["checkpoint"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tags_only_point_at_main() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let a = root.join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (_serve, base) = start_serve(&root.join("serve-data")).await;

    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("README.md"), "hello\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit");
    ok(oak(&home, &a, &["desc", "first"]), "desc");
    ok(
        oak(
            &home,
            &a,
            &["push", "--repo", "oak/main-only", "-r", &base, "--json"],
        ),
        "push",
    );
    ok(oak(&home, &a, &["merge", "--json"]), "merge");

    // A commit that only exists on a pushed branch cannot be tagged.
    ok(oak(&home, &a, &["switch", "-c", "feature"]), "switch");
    std::fs::write(a.join("feature.txt"), "wip\n").unwrap();
    ok(
        oak(&home, &a, &["commit", "--push", "--json"]),
        "commit push",
    );
    let head = String::from_utf8(ok(oak(&home, &a, &["hash"]), "hash").stdout)
        .unwrap()
        .trim()
        .to_string();
    let refused = oak(
        &home,
        &a,
        &["release", "create", "wip", "--commit", &head, "--json"],
    );
    assert_eq!(
        refused.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let body = String::from_utf8_lossy(&refused.stdout);
    assert!(body.contains("not on main"), "{body}");

    // Drafts exist on the server but are not synced by pull.
    ok(
        oak(&home, &a, &["release", "create", "v2", "--draft", "--json"]),
        "draft",
    );
    ok(oak(&home, &a, &["pull", "--json"]), "pull");
    let cached = json(&ok(
        oak(&home, &a, &["release", "list", "--offline", "--json"]),
        "offline",
    ));
    assert_eq!(cached["tags"], serde_json::json!([]));
    let remote = json(&ok(oak(&home, &a, &["release", "list", "--json"]), "list"));
    assert_eq!(remote["tags"][0]["draft"], true);
}

/// A shallow clone holds only main's head, so a tag one commit behind it names
/// a commit that is not local. The tag cache has to accept that: the clone
/// succeeds, the tag stays listed, and a later pull keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_clone_keeps_a_tag_behind_head() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let a = root.join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (_serve, base) = start_serve(&root.join("serve-data")).await;

    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("README.md"), "one\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit");
    ok(oak(&home, &a, &["desc", "first"]), "desc");
    ok(
        oak(
            &home,
            &a,
            &["push", "--repo", "oak/shallow", "-r", &base, "--json"],
        ),
        "push",
    );
    ok(oak(&home, &a, &["merge", "--json"]), "merge");
    let created = json(&ok(
        oak(&home, &a, &["release", "create", "v1.0.0", "--json"]),
        "release create",
    ));
    let tagged = created["tag"]["commit"].as_str().unwrap().to_string();

    // Move main past the tagged commit.
    ok(oak(&home, &a, &["switch", "-c", "second"]), "switch");
    std::fs::write(a.join("README.md"), "two\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit two");
    ok(oak(&home, &a, &["desc", "second"]), "desc two");
    ok(oak(&home, &a, &["push", "--json"]), "push two");
    ok(oak(&home, &a, &["merge", "--json"]), "merge two");

    let url = format!("{base}/oak/shallow");
    let dest = root.join("shallow");
    ok(
        oak(
            &home,
            &root,
            &["clone", &url, dest.to_str().unwrap(), "--shallow", "--json"],
        ),
        "shallow clone",
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("README.md")).unwrap(),
        "two\n"
    );

    // The precondition this test exists for: the tagged commit really is
    // absent from the shallow clone.
    {
        let repo = SqliteRepository::open(&dest.join(".oak/oak.db")).unwrap();
        assert!(
            repo.get_commit(&Hash(tagged.clone())).unwrap().is_none(),
            "shallow clone should not hold the tagged commit"
        );
    }

    let listed = json(&ok(
        oak(&home, &dest, &["release", "list", "--offline", "--json"]),
        "release list",
    ));
    assert_eq!(listed["tags"][0]["name"], "v1.0.0", "{listed}");
    assert_eq!(listed["tags"][0]["commit"], tagged.as_str(), "{listed}");

    ok(oak(&home, &dest, &["pull", "--json"]), "pull");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.test")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.test")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_tags_survive_clone_push_and_export() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (_serve, base) = start_serve(&root.join("serve-data")).await;

    // A git repo with a lightweight and an annotated tag on main, and a tag
    // on a side branch that only a merge's second parent reaches.
    let src = root.join("src.git");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("a.txt"), "a").unwrap();
    git(&src, &["add", "a.txt"]);
    git(&src, &["commit", "-q", "-m", "a"]);
    git(&src, &["tag", "v0.1.0"]);
    git(&src, &["checkout", "-q", "-b", "side"]);
    std::fs::write(src.join("s.txt"), "s").unwrap();
    git(&src, &["add", "s.txt"]);
    git(&src, &["commit", "-q", "-m", "s"]);
    git(&src, &["tag", "side-only"]);
    git(&src, &["checkout", "-q", "main"]);
    std::fs::write(src.join("b.txt"), "b").unwrap();
    git(&src, &["add", "b.txt"]);
    git(&src, &["commit", "-q", "-m", "b"]);
    git(&src, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    git(&src, &["tag", "-a", "v0.2.0", "-m", "second"]);

    // Clone keeps the two tags on main as local-only tags.
    ok(
        oak(&home, &root, &["clone", src.to_str().unwrap(), "imported"]),
        "git clone",
    );
    let checkout = root.join("imported");
    let local = json(&ok(
        oak(
            &home,
            &checkout,
            &["release", "list", "--offline", "--json"],
        ),
        "offline list",
    ));
    let mut names: Vec<&str> = local["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["v0.1.0", "v0.2.0"], "side-only is not on main");
    // Server commands explain that the checkout isn't linked yet.
    let unlinked = oak(&home, &checkout, &["release", "list", "--json"]);
    assert!(!unlinked.status.success());
    assert!(String::from_utf8_lossy(&unlinked.stdout).contains("oak push --repo"));

    // Publish main, then the tags.
    ok(
        oak(
            &home,
            &checkout,
            &["push", "--repo", "oak/imported", "-r", &base, "--json"],
        ),
        "push",
    );
    let pushed = json(&ok(
        oak(&home, &checkout, &["release", "push", "--json"]),
        "release push",
    ));
    let mut created: Vec<&str> = pushed["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["outcome"] == "created")
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    created.sort();
    assert_eq!(created, vec!["v0.1.0", "v0.2.0"], "{pushed}");

    // Give v0.2.0 release notes, then export back to git.
    ok(
        oak(
            &home,
            &checkout,
            &[
                "release",
                "edit",
                "v0.2.0",
                "--title",
                "Second",
                "--notes",
                "Notes here",
                "--json",
            ],
        ),
        "edit",
    );
    let out = root.join("exported");
    ok(
        oak(&home, &checkout, &["export", out.to_str().unwrap()]),
        "export",
    );
    let tags = git(&out, &["tag", "-l"]);
    assert_eq!(tags.lines().collect::<Vec<_>>(), vec!["v0.1.0", "v0.2.0"]);
    assert_eq!(
        git(&out, &["cat-file", "-t", "v0.1.0"]),
        "commit",
        "a tag with no title or notes stays lightweight"
    );
    assert_eq!(
        git(&out, &["cat-file", "-t", "v0.2.0"]),
        "tag",
        "releases become annotated"
    );
    let message = git(&out, &["tag", "-l", "--format=%(contents)", "v0.2.0"]);
    assert!(
        message.starts_with("Second") && message.contains("Notes here"),
        "{message}"
    );
    // Each tag points at the matching replayed commit.
    assert_eq!(
        git(&out, &["rev-list", "--count", "v0.1.0^{commit}"]),
        "1",
        "v0.1.0 is the first commit"
    );
    assert_eq!(
        git(&out, &["rev-list", "--count", "v0.2.0^{commit}"]),
        git(&out, &["rev-list", "--count", "HEAD"])
    );
}
