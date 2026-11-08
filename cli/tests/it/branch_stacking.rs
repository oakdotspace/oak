//! Branch stacking end to end, through the real `oak` binary against
//! `oak serve`: `oak switch -c --stack`, syncing a child from its stacked
//! parent, the bottom-up landing flow, and server-side restacking.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    remote: String,
}

impl Harness {
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let remote = oak_cli::commands::serve::spawn_loopback(root.join("data"))
            .await
            .unwrap();
        Self {
            _temp: temp,
            root,
            home,
            remote,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Run `oak` in `dir`, isolated from the caller's credentials and remotes.
    fn oak(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env("OAK_PROGRESS", "never")
            .env("NO_COLOR", "1")
            .env_remove("OAK_API_KEY")
            .env_remove("OAK_REMOTE")
            .env_remove("OAK_REPO")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("oak binary should run")
    }

    /// Run `oak` and require success, returning stdout.
    fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.oak(dir, args);
        assert!(
            out.status.success(),
            "`oak {}` failed ({:?})\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run `oak ... --json` and parse the last JSON document on stdout.
    fn json(&self, dir: &Path, args: &[&str]) -> Value {
        let stdout = self.ok(dir, args);
        parse_last_json(&stdout)
    }

    /// `oak init` + one commit + first publish to `acme/demo`. Returns the
    /// publishing checkout.
    fn publish_seed_repo(&self, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.path("seed");
        std::fs::create_dir_all(&dir).unwrap();
        self.ok(&dir, &["init"]);
        for (path, content) in files {
            write(&dir, path, content);
        }
        self.ok(&dir, &["commit"]);
        self.ok(
            &dir,
            &["push", "-r", &self.remote, "--repo", "acme/demo", "--json"],
        );
        dir
    }

    fn clone(&self, dest: &str, extra: &[&str]) -> PathBuf {
        let url = format!("{}/acme/demo", self.remote);
        let mut args = vec!["clone", url.as_str(), dest];
        args.extend_from_slice(extra);
        self.ok(&self.root, &args);
        self.path(dest)
    }

    async fn api(&self, path: &str) -> Value {
        let url = format!("{}{path}", self.remote);
        let response = reqwest::get(&url).await.unwrap();
        assert!(
            response.status().is_success(),
            "GET {url}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

fn parse_last_json(stdout: &str) -> Value {
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .or_else(|| serde_json::from_str(stdout.trim()).ok())
        .unwrap_or_else(|| panic!("no JSON document on stdout:\n{stdout}"))
}

fn current_branch(h: &Harness, dir: &Path) -> String {
    h.json(dir, &["info", "--json"])["branch"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Publish a seed repo and land it on `main`, the state every other test
/// starts from. Returns the publishing checkout (now on a fresh branch).
fn seeded_main(h: &Harness) -> PathBuf {
    let seed = h.publish_seed_repo(&[("README.md", "hello\n"), ("src/main.rs", "fn main() {}\n")]);
    h.ok(&seed, &["desc", "initial import"]);
    let merged = h.json(&seed, &["merge", "--json"]);
    assert_eq!(merged["merged"], true, "{merged}");
    seed
}

/// Start a branch in `dir`, write `files`, describe, commit, and push it.
fn push_branch(h: &Harness, dir: &Path, branch: &str, files: &[(&str, &str)]) {
    h.ok(dir, &["switch", "-c", branch]);
    commit_and_push(h, dir, branch, files);
}

fn commit_and_push(h: &Harness, dir: &Path, branch: &str, files: &[(&str, &str)]) {
    for (path, content) in files {
        write(dir, path, content);
    }
    h.ok(dir, &["desc", &format!("{branch} work")]);
    h.ok(dir, &["commit"]);
    h.ok(dir, &["push", "--json"]);
}

fn parent_of(h: &Harness, dir: &Path) -> String {
    h.json(dir, &["info", "--json"])["parent"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn remote_branch(h: &Harness, name: &str) -> Value {
    h.api(&format!("/api/acme/demo/branches/{name}")).await
}

fn stderr_of(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The whole stack lifecycle: stack a branch on another, push both, keep the
/// child in sync with its moving parent from a second checkout, land the
/// bottom, watch the server restack the child onto main, and land the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_push_sync_land_bottom_up() {
    let h = Harness::start().await;
    seeded_main(&h);
    let b = h.clone("b", &[]);

    push_branch(&h, &b, "base", &[("base.txt", "base v1\n")]);
    h.ok(&b, &["switch", "-c", "top", "--stack"]);
    assert_eq!(current_branch(&h, &b), "top");
    assert_eq!(parent_of(&h, &b), "base");
    assert_eq!(
        h.json(&b, &["info", "--json"])["stack"],
        serde_json::json!(["main", "base", "top"])
    );
    assert_eq!(
        read(&b, "base.txt"),
        "base v1\n",
        "starts at the parent's head"
    );
    commit_and_push(&h, &b, "top", &[("top.txt", "top\n")]);
    assert_eq!(remote_branch(&h, "top").await["parent_branch"], "base");

    // The branch diff is the child's own work only.
    let diff = h.ok(&b, &["diff", "--branch", "--stat"]);
    assert!(diff.contains("top.txt"), "{diff}");
    assert!(
        !diff.contains("base.txt"),
        "parent's work leaked into the child diff: {diff}"
    );

    // A stacked branch can't land directly.
    let refused = h.oak(&b, &["merge"]);
    assert!(!refused.status.success());
    let msg = stderr_of(&refused);
    assert!(msg.contains("stacked on 'base'"), "{msg}");
    assert!(msg.contains("merge 'base' first"), "{msg}");

    // A second checkout picks up the stack, and `oak pull` there follows the
    // parent as it moves on the server.
    let c = h.clone("c", &[]);
    h.ok(&c, &["switch", "top"]);
    assert_eq!(parent_of(&h, &c), "base");
    h.ok(&b, &["switch", "base"]);
    commit_and_push(&h, &b, "base", &[("base.txt", "base v2\n")]);
    h.ok(&c, &["pull", "--json"]);
    assert_eq!(
        read(&c, "base.txt"),
        "base v2\n",
        "child synced from its stacked parent"
    );
    assert_eq!(read(&c, "top.txt"), "top\n");
    h.ok(&c, &["push", "--json"]);

    // Land the bottom: the server restacks `top` onto main.
    let landed = h.json(&b, &["merge", "--json"]);
    assert_eq!(landed["merged"], true, "{landed}");
    let top = remote_branch(&h, "top").await;
    assert_eq!(top["parent_branch"], "main", "{top}");
    assert_eq!(top["status"], "open");

    // `oak pull` in the child's checkout adopts the new parent and merges
    // main in; then the child lands like any other branch.
    let pulled = h.json(&c, &["pull", "--json"]);
    assert_eq!(pulled["parent"], "main", "{pulled}");
    assert_eq!(pulled["restacked_from"], "base", "{pulled}");
    assert_eq!(parent_of(&h, &c), "main");
    let diff = h.ok(&c, &["diff", "--branch", "--stat"]);
    assert!(
        diff.contains("top.txt") && !diff.contains("base.txt"),
        "{diff}"
    );
    let landed = h.json(&c, &["merge", "--json"]);
    assert_eq!(landed["merged"], true, "{landed}");

    let fresh = h.clone("d", &[]);
    assert_eq!(read(&fresh, "base.txt"), "base v2\n");
    assert_eq!(read(&fresh, "top.txt"), "top\n");
}

/// Bad parents are refused locally and by the server, with a next step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_rejects_bad_parents() {
    let h = Harness::start().await;
    seeded_main(&h);
    let b = h.clone("b", &[]);

    let out = h.oak(&b, &["switch", "-c", "x", "--parent", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no such local branch"),
        "{}",
        stderr_of(&out)
    );

    // `--parent main` is just an ordinary branch.
    h.ok(&b, &["switch", "-c", "plain", "--parent", "main"]);
    assert_eq!(parent_of(&h, &b), "main");

    // A child of a never-pushed parent can't be pushed until the parent is.
    write(&b, "local.txt", "local\n");
    h.ok(&b, &["commit"]);
    h.ok(&b, &["switch", "-c", "child", "--stack"]);
    write(&b, "child.txt", "child\n");
    h.ok(&b, &["commit"]);
    let out = h.oak(&b, &["push", "--json"]);
    assert!(
        !out.status.success(),
        "push of an orphaned stack should fail"
    );
    let msg = stderr_of(&out);
    assert!(msg.contains("Push 'plain' first"), "{msg}");

    h.ok(&b, &["switch", "plain"]);
    h.ok(&b, &["push", "--json"]);
    h.ok(&b, &["switch", "child"]);
    h.ok(&b, &["push", "--json"]);
}

/// Closing the middle of a stack restacks the branch above it onto the
/// closed branch's own parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_middle_restacks_the_top() {
    let h = Harness::start().await;
    seeded_main(&h);
    let b = h.clone("b", &[]);
    push_branch(&h, &b, "low", &[("low.txt", "low\n")]);
    h.ok(&b, &["switch", "-c", "mid", "--stack"]);
    commit_and_push(&h, &b, "mid", &[("mid.txt", "mid\n")]);
    h.ok(&b, &["switch", "-c", "high", "--stack"]);
    commit_and_push(&h, &b, "high", &[("high.txt", "high\n")]);
    assert_eq!(remote_branch(&h, "high").await["parent_branch"], "mid");

    h.ok(&b, &["close", "mid"]);
    let high = remote_branch(&h, "high").await;
    assert_eq!(high["parent_branch"], "low", "{high}");
    h.ok(&b, &["switch", "high"]);
    h.ok(&b, &["pull", "--json"]);
    assert_eq!(parent_of(&h, &b), "low");
}
