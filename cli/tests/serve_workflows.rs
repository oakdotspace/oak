//! `oak serve` end to end: the real `oak` binary against the reference server.
//!
//! `oak serve` is meant to be a self-hosted stand-in for oak.space, so each
//! test here drives a whole user workflow — clone, branch, push, review,
//! rename, close, merge, pull — through the CLI exactly as a person would,
//! with no wiremock in between. The protocol-level edge cases (staged push,
//! mapping proofs, pull depth) live in `serve.rs`'s unit tests; these pin the
//! collaboration surface the CLI depends on.

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
    for (path, content) in files {
        write(dir, path, content);
    }
    h.ok(dir, &["desc", &format!("{branch} work")]);
    h.ok(dir, &["commit"]);
    h.ok(dir, &["push", "--json"]);
}

/// The everyday loop: clone by the URL `oak serve` prints, branch, push,
/// merge onto `main`, and pull the result into another checkout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_by_url_push_merge_and_pull() {
    let h = Harness::start().await;
    let seed = seeded_main(&h);

    let b = h.clone("b", &[]);
    assert_eq!(read(&b, "README.md"), "hello\n");
    push_branch(&h, &b, "feature", &[("feature.txt", "new\n")]);

    let listed = h.json(&seed, &["branch", "list", "--remote", "--json"]);
    let feature = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "feature")
        .unwrap_or_else(|| panic!("feature not listed: {listed}"));
    assert_eq!(feature["status"], "open");

    let merged = h.json(&b, &["merge", "--json"]);
    assert_eq!(merged["message"], "feature work");
    assert_ne!(merged["new_branch"], "feature");

    h.ok(&seed, &["pull", "--json"]);
    assert_eq!(read(&seed, "feature.txt"), "new\n");

    let log = h.json(&seed, &["log", "--remote", "--json", "-n", "5"]);
    let subjects: Vec<_> = log["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| {
            commit["description_or_subject"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(subjects, vec!["feature work", "initial import"]);

    let listed = h.json(&seed, &["branch", "list", "--remote", "--json"]);
    let feature = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "feature")
        .unwrap();
    assert_eq!(feature["status"], "closed", "merge closes the branch");
}

/// Overlapping edits are refused (exit 5) until the branch merges `main` in
/// and resolves; the resolution then lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_merge_lands_after_resolution() {
    let h = Harness::start().await;
    seeded_main(&h);
    let b = h.clone("b", &[]);
    let c = h.clone("c", &[]);
    // Both branches fork from the same `main` before either lands.
    push_branch(&h, &b, "ours", &[("README.md", "ours\n")]);
    push_branch(&h, &c, "theirs", &[("README.md", "theirs\n")]);
    h.json(&b, &["merge", "--json"]);

    let refused = h.oak(&c, &["merge", "--json"]);
    assert_eq!(refused.status.code(), Some(5), "{refused:?}");
    let pulled = h.oak(&c, &["pull"]);
    assert_eq!(pulled.status.code(), Some(5), "{pulled:?}");
    write(&c, "README.md", "ours and theirs\n");
    h.ok(&c, &["pull", "--continue"]);
    h.ok(&c, &["push"]);
    assert_eq!(h.json(&c, &["merge", "--json"])["merged"], true);

    let d = h.clone("d", &[]);
    assert_eq!(read(&d, "README.md"), "ours and theirs\n");
}

/// Rename syncs to the server and stays clonable; remote review and fenced
/// remote close work against the live listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_review_and_remote_close() {
    let h = Harness::start().await;
    let seed = seeded_main(&h);
    push_branch(&h, &seed, "topic", &[("topic.txt", "topic\n")]);
    let renamed = h.ok(&seed, &["branch", "rename", "topic", "topic2"]);
    assert!(renamed.contains("synced to remote"), "{renamed}");
    write(&seed, "topic.txt", "topic, renamed\n");
    h.ok(&seed, &["commit"]);
    h.ok(&seed, &["push"]);

    let fresh = h.clone("fresh", &[]);
    h.ok(&fresh, &["switch", "topic2"]);
    assert_eq!(read(&fresh, "topic.txt"), "topic, renamed\n");

    let review = h.json(
        &fresh,
        &[
            "branch",
            "review",
            "topic2",
            "--remote",
            "--merge-preview",
            "--json",
        ],
    );
    assert_eq!(review["schema_version"], 1, "{review}");

    let head = h.json(&seed, &["info", "--json"])["head"]
        .as_str()
        .unwrap()
        .to_string();
    let closed = h.json(
        &fresh,
        &[
            "close",
            "--remote",
            &format!("topic2@{head}"),
            "--reason",
            "superseded",
            "--json",
        ],
    );
    assert_eq!(closed["all_closed"], true, "{closed}");
    let listed = h.json(&fresh, &["branch", "list", "--remote", "--json"]);
    let topic = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "topic2")
        .unwrap();
    assert_eq!(topic["status"], "closed");
    assert!(!listed
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "topic"));
}

/// The account-ish commands get real answers instead of 404/405s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_list_auth_ci_and_finish() {
    let h = Harness::start().await;
    let seed = seeded_main(&h);

    let repos = h.json(&seed, &["repo", "list", "-r", &h.remote, "--json"]);
    assert_eq!(repos["count"], 1, "{repos}");
    assert_eq!(repos["repos"][0]["full_name"], "acme/demo");

    let auth = h.json(&seed, &["auth", "status", "-r", &h.remote, "--json"]);
    assert_eq!(auth["identity"]["state"], "authenticated", "{auth}");

    assert_eq!(
        h.json(&seed, &["ci", "runs", "--json"])["runs"],
        serde_json::json!([])
    );
    let status = h.oak(&seed, &["ci", "status", "--json"]);
    assert_eq!(status.status.code(), Some(1), "no runs exits 1");
    assert_eq!(
        parse_last_json(&String::from_utf8_lossy(&status.stdout))["state"],
        "no_runs"
    );

    write(&seed, "done.txt", "done\n");
    let desc = h.path("finish.txt");
    std::fs::write(&desc, "finished on serve\n").unwrap();
    let finished = h.json(
        &seed,
        &["finish", "--desc-file", desc.to_str().unwrap(), "--json"],
    );
    assert_eq!(finished["phase"], "complete", "{finished}");
    assert_eq!(finished["pushed"], true);
    let branch = current_branch(&h, &seed);
    let remote = h.api(&format!("/api/acme/demo/branches/{branch}")).await;
    assert_eq!(remote["head"], h.json(&seed, &["info", "--json"])["head"]);
}

/// Shallow, sparse, and exact-head clones are proved before download (no
/// `--allow-legacy-scope`), and the integrity diagnostics verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scoped_clones_and_integrity_diagnostics() {
    let h = Harness::start().await;
    let seed = seeded_main(&h);
    push_branch(&h, &seed, "feature", &[("src/lib.rs", "pub fn f() {}\n")]);
    let feature_head = h.json(&seed, &["info", "--json"])["head"]
        .as_str()
        .unwrap()
        .to_string();

    let shallow = h.clone("shallow", &["--shallow"]);
    assert_eq!(read(&shallow, "README.md"), "hello\n");

    let sparse = h.clone("sparse", &["--path", "src"]);
    assert!(sparse.join("src/main.rs").exists());
    assert!(
        !sparse.join("README.md").exists(),
        "out-of-cone file materialized"
    );

    let pinned = h.clone(
        "pinned",
        &["--branch", "feature", "--expected-head", &feature_head],
    );
    assert_eq!(current_branch(&h, &pinned), "feature");
    assert_eq!(read(&pinned, "src/lib.rs"), "pub fn f() {}\n");

    let stale = "0".repeat(64);
    let url = format!("{}/acme/demo", h.remote);
    let refused = h.oak(
        &h.root,
        &[
            "clone",
            &url,
            "stale",
            "--branch",
            "feature",
            "--expected-head",
            &stale,
        ],
    );
    assert!(!refused.status.success());
    assert!(!h.path("stale").exists(), "a refused clone creates nothing");

    for args in [
        vec!["doctor", "--repo", "acme/demo", "-r", &h.remote, "--json"],
        vec![
            "doctor",
            "--repo",
            "acme/demo",
            "-r",
            &h.remote,
            "--verify",
            "existence",
            "--json",
        ],
        vec![
            "doctor",
            "--repo",
            "acme/demo",
            "-r",
            &h.remote,
            "--verify",
            "bytes",
            "--depth",
            "2",
            "--max-chunks",
            "100",
            "--json",
        ],
    ] {
        let report = h.json(&seed, &args);
        assert_eq!(report["outcome"], "verified", "{args:?}: {report}");
    }

    let blob = h.json(
        &seed,
        &["file", "inspect", "--at", "HEAD", "--json", "README.md"],
    )["evidence"]["blob"]
        .as_str()
        .unwrap()
        .to_string();
    let info = h.json(
        &seed,
        &[
            "blob",
            "info",
            &blob,
            "--repo",
            "acme/demo",
            "-r",
            &h.remote,
            "--json",
        ],
    );
    assert_eq!(info["blob_evidence"][0]["hash"], blob, "{info}");
    assert_eq!(info["healthy"], true);
}

/// Kills a spawned process when the test ends, pass or fail.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A real `oak serve` process requiring the shared token `s3cret`, on an
/// OS-assigned port. Returns the process guard and the address it printed.
fn spawn_token_server(data: &Path) -> (KillOnDrop, String) {
    use std::io::{BufRead, BufReader};

    let mut server = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["serve", "--dir"])
        .arg(data)
        .args(["--port", "0"])
        .env("OAK_SERVE_TOKEN", "s3cret")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = server.stdout.take().unwrap();
    let server = KillOnDrop(server);
    let remote = BufReader::new(stdout)
        .lines()
        .map_while(std::result::Result::ok)
        .find_map(|line| {
            line.split_whitespace()
                .find(|word| word.starts_with("http://"))
                .map(str::to_string)
        })
        .expect("oak serve prints its address");
    (server, remote)
}

/// `oak` in `dir` with its own `HOME`, presenting `key` as `OAK_API_KEY` only
/// when given.
fn oak_command(home: &Path, dir: &Path, key: Option<&str>, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oak"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env("OAK_PROGRESS", "never")
        .env("NO_COLOR", "1")
        .env_remove("OAK_API_KEY")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env_remove("OAK_BROWSER")
        .stdin(std::process::Stdio::null());
    if let Some(key) = key {
        command.env("OAK_API_KEY", key);
    }
    command
}

/// Run `oak` and require success, returning stdout.
fn oak_ok(home: &Path, dir: &Path, key: Option<&str>, args: &[&str]) -> String {
    let out = oak_command(home, dir, key, args).output().unwrap();
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

/// A real `oak serve --token` process: every request needs the shared
/// token, and a clone that presented it keeps it for later commands.
#[test]
fn token_protected_server_requires_and_remembers_the_key() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (_server, remote) = spawn_token_server(&root.join("data"));

    let oak = |dir: &Path, key: Option<&str>, args: &[&str]| {
        oak_command(&home, dir, key, args).output().unwrap()
    };
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    write(&seed, "README.md", "hello\n");
    assert!(oak(&seed, None, &["init"]).status.success());
    assert!(oak(&seed, None, &["commit"]).status.success());
    let push = ["push", "-r", remote.as_str(), "--repo", "acme/demo"];
    let refused = oak(&seed, None, &push);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("401"));
    assert!(!oak(&seed, Some("wrong"), &push).status.success());
    assert!(oak(&seed, Some("s3cret"), &push).status.success());
    assert!(oak(&seed, Some("s3cret"), &["desc", "init"])
        .status
        .success());
    assert!(oak(&seed, Some("s3cret"), &["merge"]).status.success());

    let url = format!("{remote}/acme/demo");
    assert!(!oak(&root, None, &["clone", &url, "nokey"]).status.success());
    let cloned = oak(&root, Some("s3cret"), &["clone", &url, "withkey"]);
    assert!(cloned.status.success(), "{cloned:?}");
    let pulled = oak(&root.join("withkey"), None, &["pull"]);
    assert!(
        pulled.status.success(),
        "the accepted key was remembered: {pulled:?}"
    );
}

/// `oak login` against a token-protected server: the browser page takes the
/// shared token once, and every later command runs on the stored credential
/// with no `OAK_API_KEY`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_through_the_browser_page_replaces_oak_api_key() {
    use std::io::{BufRead, BufReader};

    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (_server, remote) = spawn_token_server(&root.join("data"));
    let remote = remote.as_str();
    // The operator holds the token; the user only ever logs in.
    let (operator, user) = (root.join("operator"), root.join("user"));
    std::fs::create_dir_all(&operator).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    let key = Some("s3cret");

    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    write(&seed, "README.md", "hello\n");
    oak_ok(&operator, &seed, None, &["init"]);
    oak_ok(&operator, &seed, None, &["commit"]);
    oak_ok(
        &operator,
        &seed,
        key,
        &["push", "-r", remote, "--repo", "acme/demo"],
    );
    oak_ok(&operator, &seed, key, &["desc", "initial import"]);
    oak_ok(&operator, &seed, key, &["merge"]);

    // `OAK_BROWSER=true` "opens" the page without a real browser, so the test
    // plays the browser: load the page, submit the token, follow the redirect
    // to the CLI's loopback listener.
    let mut login = oak_command(&user, &root, None, &["login", "-r", remote]);
    login
        .env("OAK_BROWSER", "true")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut login = KillOnDrop(login.spawn().unwrap());
    let mut lines = BufReader::new(login.0.stdout.take().unwrap())
        .lines()
        .map_while(std::result::Result::ok);
    let page_url = lines
        .by_ref()
        .find_map(|line| {
            line.split_whitespace()
                .find(|word| word.starts_with(&format!("{remote}/cli-auth?")))
                .map(str::to_string)
        })
        .expect("oak login prints the page it opens");
    let param = |name: &str| {
        let query = page_url.split_once('?').unwrap().1;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .map(|value| urlencoding::decode(value).unwrap().into_owned())
            .unwrap_or_else(|| panic!("{name} missing from {page_url}"))
    };
    let (callback, state) = (param("callback"), param("state"));
    assert!(callback.starts_with("http://127.0.0.1:"), "{callback}");

    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let page = browser.get(&page_url).send().await.unwrap();
    assert_eq!(page.status(), 200, "the page needs no bearer token");
    assert!(page.text().await.unwrap().contains("<form"));
    let submit = |token: &'static str| {
        browser
            .post(format!("{remote}/cli-auth"))
            .form(&[
                ("token", token),
                ("username", "alice"),
                ("callback", callback.as_str()),
                ("state", state.as_str()),
            ])
            .send()
    };
    let wrong = submit("wrong").await.unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(wrong.headers().get("location").is_none());
    let authorized = submit("s3cret").await.unwrap();
    assert_eq!(authorized.status(), 303);
    let location = authorized.headers()["location"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(location.starts_with(&format!("{callback}/?")), "{location}");
    let landed = browser.get(&location).send().await.unwrap();
    assert_eq!(landed.status(), 200);
    assert!(landed.text().await.unwrap().contains("signed in"));

    let rest: Vec<String> = lines.collect();
    let status = login.0.wait().unwrap();
    assert!(status.success(), "oak login failed: {rest:?}");
    assert!(
        rest.iter()
            .any(|line| line.contains("Logged in as 'alice'")),
        "{rest:?}"
    );

    // Nothing below presents OAK_API_KEY.
    assert_eq!(
        oak_ok(&user, &root, None, &["whoami", "-r", remote]).trim(),
        "alice"
    );
    let url = format!("{remote}/acme/demo");
    oak_ok(&user, &root, None, &["clone", &url, "checkout"]);
    let checkout = root.join("checkout");
    assert_eq!(read(&checkout, "README.md"), "hello\n");

    write(&seed, "NEWS.md", "news\n");
    oak_ok(&operator, &seed, key, &["desc", "news"]);
    oak_ok(&operator, &seed, key, &["commit"]);
    oak_ok(&operator, &seed, key, &["push"]);
    oak_ok(&operator, &seed, key, &["merge"]);
    oak_ok(&user, &checkout, None, &["pull"]);
    assert_eq!(read(&checkout, "NEWS.md"), "news\n");
}
