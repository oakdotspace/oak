//! `oak start` + `oak watch` end to end against `oak serve`.
//!
//! `oak serve` has neither the event stream nor the activity endpoint, so
//! these also pin the degraded path every self-hosted user gets: polling
//! (sped up with `OAK_WATCH_POLL_SECS`) and no heartbeat.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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

    fn oak_with_stdin(&self, dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env("OAK_PROGRESS", "never")
            .env("NO_COLOR", "1")
            .env("OAK_WATCH_POLL_SECS", "1")
            .env_remove("OAK_API_KEY")
            .env_remove("OAK_REMOTE")
            .env_remove("OAK_REPO")
            .env_remove("OAK_WATCH")
            .env_remove("OAK_WATCH_AUTO_PULL")
            .env_remove("OAK_WATCH_AUTO_PUSH")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("oak binary should run");
        let mut pipe = child.stdin.take().unwrap();
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.oak_with_stdin(dir, args, None);
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

    fn json(&self, dir: &Path, args: &[&str]) -> Value {
        let stdout = self.ok(dir, args);
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("`oak {}` JSON ({e}):\n{stdout}", args.join(" ")))
    }

    fn hook(&self, dir: &Path, event: &str) -> Option<Value> {
        let out = self.oak_with_stdin(dir, &["watch", "hook", event], Some("{}"));
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        (!stdout.trim().is_empty()).then(|| serde_json::from_str(stdout.trim()).unwrap())
    }

    async fn api(&self, path: &str) -> Value {
        reqwest::get(format!("{}{path}", self.remote))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Seed `acme/demo` with `files` on main; returns the seed checkout.
    fn seeded_main(&self, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.root.join("seed");
        std::fs::create_dir_all(&dir).unwrap();
        self.ok(&dir, &["init"]);
        for (path, content) in files {
            std::fs::write(dir.join(path), content).unwrap();
        }
        self.ok(&dir, &["commit"]);
        self.ok(
            &dir,
            &["push", "-r", &self.remote, "--repo", "acme/demo", "--json"],
        );
        self.ok(&dir, &["desc", "initial import"]);
        self.ok(&dir, &["merge", "--json"]);
        dir
    }

    /// Land a change to main from `seed`.
    fn merge_to_main(&self, seed: &Path, path: &str, content: &str) {
        self.ok(seed, &["pull"]);
        std::fs::write(seed.join(path), content).unwrap();
        self.ok(seed, &["commit"]);
        self.ok(seed, &["desc", "main change"]);
        self.ok(seed, &["merge", "--json"]);
    }
}

/// Stops the watch daemon when a test ends, pass or fail.
struct StopWatch<'a>(&'a Harness, PathBuf);

impl Drop for StopWatch<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .oak_with_stdin(&self.1, &["watch", "stop", "--json"], None);
    }
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_publishes_and_watch_merges_main_pushes_and_reports() {
    let h = Harness::start().await;
    let seed = h.seeded_main(&[("a.txt", "one\n"), ("b.txt", "base\n")]);
    h.ok(
        &h.root,
        &["clone", &format!("{}/acme/demo", h.remote), "agent"],
    );
    let agent = h.root.join("agent");
    let _stop = StopWatch(&h, agent.clone());

    let started = h.json(&agent, &["start", "Fix the login redirect", "--json"]);
    assert_eq!(started["branch"], "fix-the-login-redirect", "{started}");
    assert_eq!(started["pushed"], true, "{started}");
    assert_eq!(started["watch_running"], true, "{started}");
    let branch = h
        .api("/api/acme/demo/branches/fix-the-login-redirect")
        .await;
    assert_eq!(branch["description"], "Fix the login redirect", "{branch}");

    // A second start with the same summary gets a deduplicated name.
    let again = h.json(
        &agent,
        &[
            "start",
            "Fix the login redirect",
            "--no-push",
            "--no-watch",
            "--json",
        ],
    );
    assert_eq!(again["branch"], "fix-the-login-redirect-2", "{again}");
    assert_eq!(again["push_skipped_reason"], "--no-push", "{again}");
    h.ok(&agent, &["switch", "fix-the-login-redirect"]);

    let status = h.json(&agent, &["watch", "status", "--json"]);
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["transport"], "poll", "oak serve has no event stream");

    // Dirty work is checkpointed, main is merged in, and the agent hears
    // about it once.
    std::fs::write(agent.join("mine.txt"), "agent work\n").unwrap();
    h.merge_to_main(&seed, "b.txt", "from main\n");
    wait_for("main to be merged in", || {
        std::fs::read_to_string(agent.join("b.txt")).is_ok_and(|b| b == "from main\n")
    })
    .await;
    let notice = h.hook(&agent, "PostToolUse").expect("a merge notice");
    let context = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("b.txt"), "{context}");
    assert_eq!(notice["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(h.hook(&agent, "PostToolUse").is_none(), "delivered once");

    // The checkpoint + sync commit are auto-pushed.
    wait_for("the merge to be pushed", || {
        let status = h.json(&agent, &["watch", "status", "--json"]);
        let local = h.json(&agent, &["status", "--json"]);
        status["last_pushed_head"] == local["head"]
    })
    .await;
    let remote = h
        .api("/api/acme/demo/branches/fix-the-login-redirect")
        .await;
    let local = h.json(&agent, &["status", "--json"]);
    assert_eq!(remote["head"], local["head"]);
    assert_eq!(local["working_changes"]["changes"], Value::Array(vec![]));

    // A conflicting change on main pauses auto-pull with a CONFLICT notice.
    std::fs::write(agent.join("b.txt"), "agent edit\n").unwrap();
    h.ok(&agent, &["commit"]);
    h.merge_to_main(&seed, "b.txt", "main edit\n");
    wait_for("the conflict", || agent.join(".oak/SYNC_HEAD").exists()).await;
    let notice = h
        .hook(&agent, "UserPromptSubmit")
        .expect("a conflict notice");
    let context = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("CONFLICT"), "{context}");
    assert!(context.contains("oak pull --continue"), "{context}");
    let reminder = h.hook(&agent, "UserPromptSubmit").expect("a reminder");
    assert!(reminder.to_string().contains("still conflicted"));

    std::fs::write(agent.join("b.txt"), "resolved\n").unwrap();
    h.ok(&agent, &["pull", "--continue"]);
    wait_for("the resolution to be pushed", || {
        let status = h.json(&agent, &["watch", "status", "--json"]);
        let local = h.json(&agent, &["status", "--json"]);
        status["conflict"].is_null() && status["last_pushed_head"] == local["head"]
    })
    .await;

    // `oak finish` stops watch.
    h.ok(&agent, &["finish", "--desc", "done", "--json"]);
    let status = h.json(&agent, &["watch", "status", "--json"]);
    assert_eq!(status["running"], false, "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_disables_watch_and_hooks_respect_it() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    h.ok(
        &h.root,
        &["clone", &format!("{}/acme/demo", h.remote), "agent"],
    );
    let agent = h.root.join("agent");
    let _stop = StopWatch(&h, agent.clone());

    h.ok(
        &agent,
        &["config", "set", "watch.enabled", "false", "--global"],
    );
    let listed = h.json(&agent, &["config", "list", "--json"]);
    assert_eq!(listed["keys"][0]["key"], "watch.enabled");
    assert_eq!(listed["keys"][0]["value"], false);
    assert_eq!(listed["keys"][0]["source"], "global");

    let started = h.json(&agent, &["start", "Quiet task", "--json"]);
    assert_eq!(started["watch_running"], false, "{started}");
    assert!(started["watch_skipped_reason"]
        .as_str()
        .unwrap()
        .contains("watch.enabled = false"));
    let brief = h.hook(&agent, "SessionStart").expect("session brief");
    assert!(brief.to_string().contains("disabled"), "{brief}");
    assert_eq!(
        h.json(&agent, &["watch", "status", "--json"])["running"],
        false
    );

    // Repo config overrides global; the SessionStart hook then starts watch.
    h.ok(&agent, &["config", "set", "watch.enabled", "true"]);
    assert_eq!(
        h.json(&agent, &["config", "get", "watch.enabled", "--json"])["source"],
        "repo"
    );
    let brief = h.hook(&agent, "SessionStart").expect("session brief");
    assert!(brief.to_string().contains("oak start"), "{brief}");
    wait_for("the hook-spawned daemon", || {
        h.json(&agent, &["watch", "status", "--json"])["running"] == true
    })
    .await;
    assert!(h.hook(&agent, "SessionEnd").is_none());
    wait_for("SessionEnd to stop watch", || {
        h.json(&agent, &["watch", "status", "--json"])["running"] == false
    })
    .await;

    let env = h.json(&agent, &["environment", "--json"]);
    assert_eq!(env["agent_sync"]["settings"]["enabled"]["source"], "repo");
}
