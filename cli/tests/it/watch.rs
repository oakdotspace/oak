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
        self.oak_env(dir, args, stdin, &[])
    }

    fn oak_env(
        &self,
        dir: &Path,
        args: &[&str],
        stdin: Option<&str>,
        envs: &[(&str, &str)],
    ) -> Output {
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
            .env_remove("OAK_MOUNTS_ROOT")
            .envs(envs.iter().copied())
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

    /// Clone `acme/demo` into `name` and start a described, pushed task
    /// branch there without starting watch. Returns the checkout and branch.
    fn task(&self, name: &str, summary: &str) -> (PathBuf, String) {
        self.ok(
            &self.root,
            &["clone", &format!("{}/acme/demo", self.remote), name],
        );
        let dir = self.root.join(name);
        let started = self.json(&dir, &["start", summary, "--no-watch", "--json"]);
        assert_eq!(started["pushed"], true, "{started}");
        assert_eq!(started["watch_running"], false, "{started}");
        let branch = started["branch"].as_str().unwrap().to_string();
        (dir, branch)
    }

    /// Run a hook from `cwd` with `stdin` as the hook input.
    fn hook_from(&self, cwd: &Path, event: &str, stdin: &str) -> Option<Value> {
        let out = self.oak_with_stdin(cwd, &["watch", "hook", event], Some(stdin));
        assert!(
            out.status.success(),
            "hooks always exit 0; got {:?}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        (!stdout.trim().is_empty()).then(|| {
            serde_json::from_str(stdout.trim())
                .unwrap_or_else(|e| panic!("hook output is JSON ({e}): {stdout}"))
        })
    }

    fn watch_status(&self, dir: &Path) -> Value {
        self.json(dir, &["watch", "status", "--json"])
    }

    fn head(&self, dir: &Path) -> Value {
        self.json(dir, &["status", "--json"])["head"].clone()
    }

    /// Run a command expected to fail; returns (exit code, stderr + stdout).
    fn fails(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String) {
        let out = self.oak_env(dir, args, None, envs);
        assert!(
            !out.status.success(),
            "`oak {}` should fail\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        );
        (
            out.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            ),
        )
    }

    async fn api_status(&self, method: reqwest::Method, path: &str) -> u16 {
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.remote))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
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
    // The files change mid-pull; the notice is saved once the pull returns.
    wait_for("the merge notice", || {
        h.watch_status(&agent)["unseen_notices"] != 0
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
    // SYNC_HEAD appears mid-pull; the notice is saved once the pull returns.
    wait_for("the conflict", || {
        !h.json(&agent, &["watch", "status", "--json"])["conflict"].is_null()
    })
    .await;
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

fn daemon_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".oak/watch/log")).unwrap_or_default()
}

/// How many pulls the daemon has started (one `pulling:` log line each).
fn pulls_started(dir: &Path) -> usize {
    daemon_log(dir).matches("pulling: main at").count()
}

/// The `kind` of every notice the daemon has queued, oldest first.
fn notice_kinds(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join(".oak/watch/state.json")) else {
        return Vec::new();
    };
    let state: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    state["notices"]
        .as_array()
        .map(|notices| {
            notices
                .iter()
                .map(|n| n["kind"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until a daemon spawned without waiting (a hook, `oak finish`'s
/// resume) has written its start state, which it does after installing its
/// stop-signal handlers. `oak watch start` has already waited for this.
async fn wait_until_looping(dir: &Path) {
    wait_for("the daemon to record its start", || {
        std::fs::read_to_string(dir.join(".oak/watch/state.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|state| state["pid"].is_u64())
    })
    .await;
}

/// Long enough for several 1s poll intervals to elapse, to show the daemon
/// does *not* act. Only used for negative assertions.
const SEVERAL_POLLS: Duration = Duration::from_millis(3500);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflict_pauses_auto_pull_and_resolution_or_main_moving_resumes_it() {
    let h = Harness::start().await;
    let seed = h.seeded_main(&[("a.txt", "one\n"), ("b.txt", "base\n")]);
    let (agent, branch) = h.task("agent", "Conflict task");
    let _stop = StopWatch(&h, agent.clone());
    h.ok(&agent, &["watch", "start", "--json"]);

    // The agent and main both edit b.txt: the daemon's pull conflicts.
    std::fs::write(agent.join("b.txt"), "agent edit\n").unwrap();
    h.ok(&agent, &["commit"]);
    h.merge_to_main(&seed, "b.txt", "main edit\n");
    let main_1 = h.api("/api/acme/demo/branches/main").await["head"].clone();
    wait_for("the conflict to be recorded", || {
        !h.watch_status(&agent)["conflict"].is_null()
    })
    .await;
    let status = h.watch_status(&agent);
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["sync_in_progress"], true, "{status}");
    assert_eq!(status["conflict"]["main_head"], main_1, "{status}");
    assert_eq!(status["conflict"]["paths"], serde_json::json!(["b.txt"]));
    assert_eq!(
        status["recommended_next_commands"],
        serde_json::json!(["oak conflict status", "oak pull --continue"]),
        "{status}"
    );
    assert!(agent.join(".oak/SYNC_HEAD").exists());
    assert!(notice_kinds(&agent).contains(&"conflict".to_string()));

    // Paused: with main still ahead, an unpaused daemon would re-pull on
    // every poll. It neither pulls nor pushes while the conflict is open.
    let pulls = pulls_started(&agent);
    assert_eq!(pulls, 1, "{}", daemon_log(&agent));
    let pushed = status["last_pushed_head"].clone();
    tokio::time::sleep(SEVERAL_POLLS).await;
    assert_eq!(pulls_started(&agent), pulls, "{}", daemon_log(&agent));
    let status = h.watch_status(&agent);
    assert_eq!(status["last_pushed_head"], pushed, "{status}");
    assert_eq!(status["conflict"]["main_head"], main_1, "{status}");

    // Aborting backs out the merge. Watch tells the agent and does not
    // re-create the same conflict while main stays put.
    h.ok(&agent, &["pull", "--abort"]);
    wait_for("the abort notice", || {
        notice_kinds(&agent).contains(&"pull_aborted".to_string())
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(agent.join("b.txt")).unwrap(),
        "agent edit\n"
    );
    let status = h.watch_status(&agent);
    assert!(status["conflict"].is_null(), "{status}");
    assert_eq!(status["sync_in_progress"], false, "{status}");
    tokio::time::sleep(SEVERAL_POLLS).await;
    assert_eq!(pulls_started(&agent), pulls, "{}", daemon_log(&agent));
    assert!(!agent.join(".oak/SYNC_HEAD").exists());

    // Main moving again un-skips it: the daemon merges again (and, b.txt
    // still disagreeing, conflicts again on the new main head).
    h.merge_to_main(&seed, "c.txt", "more main\n");
    let main_2 = h.api("/api/acme/demo/branches/main").await["head"].clone();
    assert_ne!(main_1, main_2);
    wait_for("the second conflict", || {
        h.watch_status(&agent)["conflict"]["main_head"] == main_2
    })
    .await;

    // Resolving resumes auto-pull: the resolution is pushed...
    std::fs::write(agent.join("b.txt"), "resolved\n").unwrap();
    h.ok(&agent, &["pull", "--continue"]);
    wait_for("the resolution to be pushed", || {
        let status = h.watch_status(&agent);
        status["conflict"].is_null() && status["last_pushed_head"] == h.head(&agent)
    })
    .await;
    // Status stops reporting the conflict once SYNC_HEAD is gone, and the push
    // can land before the daemon's next check settles it, so wait for that too.
    wait_for("the conflict to be settled", || {
        daemon_log(&agent).contains("conflict resolved; auto-pull resumed")
    })
    .await;
    let remote = h.api(&format!("/api/acme/demo/branches/{branch}")).await;
    assert_eq!(remote["head"], h.head(&agent), "{remote}");

    // ...and the next main change merges in on its own.
    h.merge_to_main(&seed, "d.txt", "later main\n");
    wait_for("the next main change to be merged in", || {
        std::fs::read_to_string(agent.join("d.txt")).is_ok_and(|d| d == "later main\n")
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(agent.join("b.txt")).unwrap(),
        "resolved\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dirty_work_is_checkpointed_before_main_is_merged_in() {
    let h = Harness::start().await;
    let seed = h.seeded_main(&[
        ("a.txt", "one\n"),
        ("b.txt", "base\n"),
        ("c.txt", "doomed\n"),
    ]);
    let (agent, branch) = h.task("agent", "Checkpoint task");
    let _stop = StopWatch(&h, agent.clone());
    h.ok(&agent, &["watch", "start", "--json"]);
    let head_before = h.head(&agent);

    // Uncommitted edits, a new file and a deletion — none of them committed.
    std::fs::write(agent.join("a.txt"), "agent edit\n").unwrap();
    std::fs::write(agent.join("new.txt"), "brand new\n").unwrap();
    std::fs::remove_file(agent.join("c.txt")).unwrap();
    h.merge_to_main(&seed, "b.txt", "from main\n");

    wait_for("main to be merged in", || {
        std::fs::read_to_string(agent.join("b.txt")).is_ok_and(|b| b == "from main\n")
    })
    .await;
    // Nothing was lost or left dirty: the work was committed, then merged.
    assert_eq!(
        std::fs::read_to_string(agent.join("a.txt")).unwrap(),
        "agent edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(agent.join("new.txt")).unwrap(),
        "brand new\n"
    );
    assert!(!agent.join("c.txt").exists());
    let local = h.json(&agent, &["status", "--json"]);
    assert_eq!(
        local["working_changes"]["changes"],
        serde_json::json!([]),
        "{local}"
    );
    assert_ne!(local["head"], head_before);
    let log = daemon_log(&agent);
    assert!(log.contains("pulling: main at"), "{log}");
    assert!(
        !notice_kinds(&agent).contains(&"checkpoint_failed".to_string()),
        "{log}"
    );

    // The checkpoint travels with the push: a fresh clone of the branch
    // sees the agent's work and main's change together.
    wait_for("the checkpoint + merge to be pushed", || {
        h.watch_status(&agent)["last_pushed_head"] == h.head(&agent)
    })
    .await;
    h.ok(
        &h.root,
        &["clone", &format!("{}/acme/demo", h.remote), "verify"],
    );
    let verify = h.root.join("verify");
    h.ok(&verify, &["switch", &branch]);
    let read = |p: &str| std::fs::read_to_string(verify.join(p)).ok();
    assert_eq!(read("a.txt").as_deref(), Some("agent edit\n"));
    assert_eq!(read("new.txt").as_deref(), Some("brand new\n"));
    assert_eq!(read("b.txt").as_deref(), Some("from main\n"));
    assert_eq!(read("c.txt"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_polling_when_events_activity_and_preferences_are_404() {
    let h = Harness::start().await;
    let seed = h.seeded_main(&[("a.txt", "one\n"), ("b.txt", "base\n")]);
    let (agent, branch) = h.task("agent", "Poll task");
    let _stop = StopWatch(&h, agent.clone());

    // Precondition: `oak serve` answers all three optional endpoints 404,
    // like an older oak.space.
    use reqwest::Method;
    assert_eq!(
        h.api_status(
            Method::GET,
            &format!("/api/acme/demo/events?branches=main,{branch}")
        )
        .await,
        404
    );
    assert_eq!(
        h.api_status(
            Method::POST,
            &format!("/api/acme/demo/branches/{branch}/activity")
        )
        .await,
        404
    );
    assert_eq!(
        h.api_status(Method::GET, "/api/user/preferences").await,
        404
    );

    // An API key makes the daemon ask for server preferences too (it skips
    // that request without one). The daemon inherits the environment.
    let out = h.oak_env(
        &agent,
        &["watch", "start", "--json"],
        None,
        &[("OAK_API_KEY", "test-key")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for("the stream and heartbeat to degrade", || {
        let status = h.watch_status(&agent);
        status["heartbeat"] == "unsupported"
            && daemon_log(&agent).contains("server has no event stream; polling")
    })
    .await;
    let status = h.watch_status(&agent);
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["transport"], "poll", "{status}");
    assert!(status.get("last_error").is_none(), "{status}");
    assert_eq!(status["settings"]["enabled"]["value"], true, "{status}");
    let log = daemon_log(&agent);
    assert!(!log.contains("could not read server preferences"), "{log}");

    // Polling still syncs both ways.
    h.merge_to_main(&seed, "b.txt", "from main\n");
    wait_for("main to be merged in by polling", || {
        std::fs::read_to_string(agent.join("b.txt")).is_ok_and(|b| b == "from main\n")
    })
    .await;
    wait_for("the merge to be pushed", || {
        h.watch_status(&agent)["last_pushed_head"] == h.head(&agent)
    })
    .await;
    assert_eq!(h.watch_status(&agent)["transport"], "poll");
}

/// `oak serve` has no working endpoint, like a server without the flag. The
/// daemon degrades without touching anything else it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sharing_work_is_off_by_default_and_degrades_on_a_404() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, _branch) = h.task("agent", "Share task");
    let _stop = StopWatch(&h, agent.clone());
    std::fs::write(agent.join("a.txt"), "two\n").unwrap();

    let status = h.watch_status(&agent);
    assert_eq!(status["settings"]["share_working"]["value"], false);
    assert_eq!(status["settings"]["share_working"]["source"], "default");
    assert!(status.get("working").is_none(), "{status}");

    let out = h.oak_env(&agent, &["watch", "stop", "--json"], None, &[]);
    assert!(out.status.success());
    let out = h.oak_env(
        &agent,
        &["watch", "start", "--json"],
        None,
        &[("OAK_WATCH_SHARE_WORKING", "1")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for("sharing to degrade", || {
        h.watch_status(&agent)["working"] == "unsupported"
    })
    .await;
    assert_eq!(h.watch_status(&agent)["running"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_refuses_mounts_and_git_backed_repositories() {
    let h = Harness::start().await;

    // Git-backed repository.
    h.ok(&h.root, &["init", "--git", "gitrepo"]);
    let git_repo = h.root.join("gitrepo");
    for args in [
        &["watch", "start"][..],
        &["watch", "start", "--json"],
        &["watch", "status", "--json"],
        &["watch", "stop"],
    ] {
        let (code, text) = h.fails(&git_repo, args, &[]);
        assert_eq!(code, Some(2), "{args:?}: {text}");
        assert!(
            text.contains("oak watch is not supported for git-backed repositories"),
            "{args:?}: {text}"
        );
    }
    assert!(!git_repo.join(".oak/watch").exists());
    // The hook stays silent there rather than failing the agent.
    assert!(h.hook_from(&git_repo, "SessionStart", "{}").is_none());

    // A registered mount (the index is what `mount_dest_for` consults),
    // including from a subdirectory of it.
    let mount = h.root.join("mounted");
    std::fs::create_dir_all(&mount).unwrap();
    h.ok(&mount, &["init"]);
    std::fs::create_dir_all(mount.join("sub")).unwrap();
    let mounts_root = h.root.join("mounts");
    std::fs::create_dir_all(&mounts_root).unwrap();
    std::fs::write(
        mounts_root.join("index.json"),
        serde_json::json!({ "mounts": { mount.display().to_string(): "m-test" } }).to_string(),
    )
    .unwrap();
    let mounts_root = mounts_root.display().to_string();
    let env = [("OAK_MOUNTS_ROOT", mounts_root.as_str())];
    for dir in [mount.clone(), mount.join("sub")] {
        for args in [
            &["watch", "start"][..],
            &["watch", "status", "--json"],
            &["watch", "stop", "--json"],
        ] {
            let (code, text) = h.fails(&dir, args, &env);
            assert_eq!(code, Some(2), "{args:?}: {text}");
            assert!(
                text.contains("oak watch runs in regular checkouts"),
                "{args:?}: {text}"
            );
        }
    }
    assert!(!mount.join(".oak/watch").exists());
    // Without the registration the same checkout is watchable.
    assert_eq!(h.watch_status(&mount)["running"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_start_needs_a_linked_checkout() {
    let h = Harness::start().await;

    // Never published: no remote to sync with.
    let local = h.root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    h.ok(&local, &["init"]);
    let (code, text) = h.fails(&local, &["watch", "start"], &[]);
    assert_ne!(code, Some(0), "{text}");
    assert!(text.contains("not linked to an Oak remote"), "{text}");
    assert!(!h.watch_status(&local)["running"].as_bool().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_status_and_stop_report_each_lifecycle_state() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, branch) = h.task("agent", "Lifecycle task");
    let _stop = StopWatch(&h, agent.clone());

    // Never started.
    let status = h.watch_status(&agent);
    assert_eq!(status["schema_version"], 1, "{status}");
    assert_eq!(status["kind"], "watch_status", "{status}");
    assert_eq!(status["running"], false, "{status}");
    assert!(status.get("pid").is_none(), "{status}");
    assert!(status.get("transport").is_none(), "{status}");
    assert!(status.get("stop_reason").is_none(), "{status}");
    assert_eq!(status["branch"], branch.as_str(), "{status}");
    assert_eq!(
        status["repository_root"],
        agent.display().to_string(),
        "{status}"
    );
    assert_eq!(status["sync_in_progress"], false, "{status}");
    assert_eq!(status["unseen_notices"], 0, "{status}");
    assert_eq!(status["settings"]["enabled"]["value"], true, "{status}");
    assert_eq!(status["settings"]["auto_pull"]["value"], true, "{status}");
    assert_eq!(status["settings"]["auto_push"]["value"], true, "{status}");
    assert!(
        status["log"].as_str().unwrap().ends_with(".oak/watch/log"),
        "{status}"
    );
    assert_eq!(
        status["recommended_next_commands"],
        serde_json::json!(["oak watch start"])
    );
    let text = h.ok(&agent, &["watch", "status"]);
    assert!(text.contains("oak watch is not running"), "{text}");
    assert!(text.contains("watch.enabled = true"), "{text}");

    let stop = h.json(&agent, &["watch", "stop", "--json"]);
    assert_eq!(
        stop,
        serde_json::json!({
            "schema_version": 1, "kind": "watch_stop",
            "was_running": false, "stopped": true, "idle_sent": false,
        })
    );
    let text = h.ok(&agent, &["watch", "stop"]);
    assert!(text.contains("oak watch was not running"), "{text}");

    // Running.
    let started = h.json(&agent, &["watch", "start", "--json"]);
    assert_eq!(started["kind"], "watch_start", "{started}");
    assert_eq!(started["running"], true, "{started}");
    assert_eq!(started["already_running"], false, "{started}");
    assert_eq!(started["branch"], branch.as_str(), "{started}");
    let pid = started["pid"].as_u64().expect("a pid");
    let again = h.json(&agent, &["watch", "start", "--json"]);
    assert_eq!(again["already_running"], true, "{again}");
    assert_eq!(again["pid"], pid, "{again}");
    let text = h.ok(&agent, &["watch", "start"]);
    assert!(text.contains("already running"), "{text}");

    let status = h.watch_status(&agent);
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["pid"], pid, "{status}");
    assert_eq!(status["transport"], "poll", "{status}");
    assert_eq!(
        status["recommended_next_commands"],
        serde_json::json!([]),
        "{status}"
    );
    let text = h.ok(&agent, &["watch", "status"]);
    assert!(
        text.contains(&format!(
            "oak watch is running (pid {pid}) on branch '{branch}' via poll"
        )),
        "{text}"
    );

    // Stopped.
    let stop = h.json(&agent, &["watch", "stop", "--json"]);
    assert_eq!(stop["was_running"], true, "{stop}");
    assert_eq!(stop["stopped"], true, "{stop}");
    assert_eq!(stop["pid"], pid, "{stop}");
    let status = h.watch_status(&agent);
    assert_eq!(status["running"], false, "{status}");
    assert!(status.get("pid").is_none(), "{status}");
    assert!(status.get("transport").is_none(), "{status}");
    assert_eq!(status["stop_reason"], "stopped", "{status}");
    let text = h.ok(&agent, &["watch", "status"]);
    assert!(
        text.contains("oak watch is not running (last stop: stopped)"),
        "{text}"
    );

    // Stale pidfile naming a live process that is not the daemon (this
    // test): the lock, not the pidfile, decides, so status reports not
    // running and stop never signals it.
    std::fs::write(agent.join(".oak/watch/pid"), std::process::id().to_string()).unwrap();
    let status = h.watch_status(&agent);
    assert_eq!(status["running"], false, "{status}");
    assert!(status.get("pid").is_none(), "{status}");
    let stop = h.json(&agent, &["watch", "stop", "--json"]);
    assert_eq!(stop["was_running"], false, "{stop}");
    assert_eq!(stop["stopped"], true, "{stop}");
    assert!(stop.get("pid").is_none(), "{stop}");
    // A state file exists, so stop tries to mark the branch idle itself;
    // `oak serve` has no activity endpoint.
    assert_eq!(stop["idle_sent"], false, "{stop}");

    // A fresh start replaces the stale pidfile.
    let started = h.json(&agent, &["watch", "start", "--json"]);
    assert_eq!(started["already_running"], false, "{started}");
    assert_ne!(started["pid"], u64::from(std::process::id()));
    assert_eq!(h.watch_status(&agent)["pid"], started["pid"]);
}

/// Seed `state.json` with a merge notice and a conflict notice for a
/// conflict that is still open (`SYNC_HEAD` present).
fn seed_notices(checkout: &Path) {
    let watch = checkout.join(".oak/watch");
    std::fs::create_dir_all(&watch).unwrap();
    std::fs::write(
        watch.join("state.json"),
        serde_json::json!({
            "schema_version": 1,
            "notices": [
                { "id": 1, "at": 0, "kind": "main_merged", "message": "merged a.txt" },
                { "id": 2, "at": 0, "kind": "conflict", "message": "CONFLICT in b.txt" },
            ],
            "next_notice_id": 3,
            "conflict": { "main_head": "abc", "paths": ["b.txt"], "at": 0 },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(checkout.join(".oak/SYNC_HEAD"), "abc").unwrap();
}

fn context(output: &Value, event: &str) -> String {
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], event);
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_session_start_briefs_the_agent_and_starts_watch() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, branch) = h.task("agent", "Hook start task");
    let _stop = StopWatch(&h, agent.clone());

    // Claude Code passes the session's cwd on stdin; the hook process runs
    // elsewhere.
    let input = serde_json::json!({ "cwd": agent }).to_string();
    let brief = h
        .hook_from(&h.root, "SessionStart", &input)
        .expect("a brief");
    let text = context(&brief, "SessionStart");
    assert!(text.contains("Start every task with `oak start"), "{text}");
    assert!(text.contains("oak pull --continue"), "{text}");
    assert!(
        text.contains(&format!("oak watch is starting for branch '{branch}'.")),
        "{text}"
    );
    wait_for("the hook-spawned daemon", || {
        h.watch_status(&agent)["running"] == true
    })
    .await;

    let brief = h.hook_from(&agent, "session-start", "{}").expect("a brief");
    let text = context(&brief, "SessionStart");
    assert!(
        text.contains(&format!("oak watch is running for branch '{branch}'.")),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_user_prompt_submit_delivers_notices_once_and_repeats_open_conflicts() {
    let h = Harness::start().await;
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    h.ok(&repo, &["init"]);
    seed_notices(&repo);
    let input = serde_json::json!({ "cwd": repo }).to_string();

    let out = h
        .hook_from(&h.root, "UserPromptSubmit", &input)
        .expect("notices");
    assert_eq!(
        context(&out, "UserPromptSubmit"),
        "merged a.txt\n\nCONFLICT in b.txt"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(".oak/watch/cursor")).unwrap(),
        "2"
    );
    // Delivered once; the open conflict is then repeated as a reminder.
    let out = h
        .hook_from(&h.root, "UserPromptSubmit", &input)
        .expect("a reminder");
    let text = context(&out, "UserPromptSubmit");
    assert!(text.starts_with("Reminder:"), "{text}");
    assert!(text.contains("still conflicted (b.txt)"), "{text}");
    assert!(!text.contains("merged a.txt"), "{text}");

    // Resolved (SYNC_HEAD gone): silence.
    std::fs::remove_file(repo.join(".oak/SYNC_HEAD")).unwrap();
    assert!(h.hook_from(&h.root, "UserPromptSubmit", &input).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_post_tool_use_delivers_notices_without_conflict_reminders() {
    let h = Harness::start().await;
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    h.ok(&repo, &["init"]);
    seed_notices(&repo);

    let out = h.hook_from(&repo, "PostToolUse", "{}").expect("notices");
    assert_eq!(
        context(&out, "PostToolUse"),
        "merged a.txt\n\nCONFLICT in b.txt"
    );
    // The conflict is still open, but tool calls are not nagged.
    assert!(repo.join(".oak/SYNC_HEAD").exists());
    assert!(h.hook_from(&repo, "PostToolUse", "{}").is_none());
    assert!(h.hook_from(&repo, "post_tool_use", "{}").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_session_end_stops_watch_silently() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, _branch) = h.task("agent", "Hook end task");
    let _stop = StopWatch(&h, agent.clone());

    // Not running: nothing to do, nothing printed.
    assert!(h.hook_from(&agent, "SessionEnd", "{}").is_none());

    h.ok(&agent, &["watch", "start", "--json"]);
    assert!(h.hook_from(&agent, "SessionEnd", "{}").is_none());
    wait_for("SessionEnd to stop watch", || {
        h.watch_status(&agent)["running"] == false
    })
    .await;
    assert_eq!(h.watch_status(&agent)["stop_reason"], "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_end_leaves_watch_running_for_another_session() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, _branch) = h.task("agent", "Two sessions");
    let _stop = StopWatch(&h, agent.clone());
    h.ok(&agent, &["watch", "start", "--json"]);

    let session = |id: &str| serde_json::json!({ "session_id": id }).to_string();
    h.hook_from(&agent, "UserPromptSubmit", &session("one"));
    h.hook_from(&agent, "UserPromptSubmit", &session("two"));

    assert!(h.hook_from(&agent, "SessionEnd", &session("one")).is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.watch_status(&agent)["running"], true);

    assert!(h.hook_from(&agent, "SessionEnd", &session("two")).is_none());
    wait_for("the last SessionEnd to stop watch", || {
        h.watch_status(&agent)["running"] == false
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_stops_when_idle_and_the_next_hook_revives_it() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, _branch) = h.task("agent", "Idle task");
    let _stop = StopWatch(&h, agent.clone());
    let start = h.oak_env(
        &agent,
        &["watch", "start", "--json"],
        None,
        &[("OAK_WATCH_IDLE_SECS", "3")],
    );
    assert!(start.status.success());

    // A session's hooks keep it alive past the timeout.
    let input = serde_json::json!({ "session_id": "alive" }).to_string();
    for _ in 0..10 {
        h.hook_from(&agent, "PostToolUse", &input);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(h.watch_status(&agent)["running"], true);

    wait_for("the idle exit", || {
        h.watch_status(&agent)["running"] == false
    })
    .await;
    let status = h.watch_status(&agent);
    assert_eq!(status["stop_reason"], "idle_timeout", "{status}");
    let state = h.json(&agent, &["agent", "state", "--json"]);
    assert_eq!(state["watch"]["stop_reason"], "idle_timeout", "{state}");

    let notice = h.hook_from(&agent, "UserPromptSubmit", &input).unwrap();
    assert!(
        context(&notice, "UserPromptSubmit").contains("without agent activity"),
        "{notice}"
    );
    wait_for("the hook to revive watch", || {
        h.watch_status(&agent)["running"] == true
    })
    .await;
    wait_until_looping(&agent).await;

    // An explicit stop is not an idle stop: hooks leave it alone.
    h.ok(&agent, &["watch", "stop"]);
    h.hook_from(&agent, "UserPromptSubmit", &input);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.watch_status(&agent)["running"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_finish_keeps_watch_and_a_real_finish_stops_it() {
    let h = Harness::start().await;
    h.seeded_main(&[("a.txt", "one\n")]);
    let (agent, _branch) = h.task("agent", "Finish task");
    let _stop = StopWatch(&h, agent.clone());
    h.ok(&agent, &["watch", "start", "--json"]);

    let sync_head = agent.join(".oak/SYNC_HEAD");
    std::fs::write(&sync_head, "x").unwrap();
    let (code, output) = h.fails(&agent, &["finish", "--desc", "done", "--json"], &[]);
    assert_ne!(code, Some(0), "{output}");
    wait_for("watch to resume after the refusal", || {
        h.watch_status(&agent)["running"] == true
    })
    .await;
    wait_until_looping(&agent).await;
    let state = h.json(&agent, &["agent", "state", "--json", "--compact"]);
    assert_eq!(state["watch"]["running"], true, "{state}");

    std::fs::remove_file(&sync_head).unwrap();
    h.ok(&agent, &["finish", "--desc", "done", "--json"]);
    assert_eq!(h.watch_status(&agent)["running"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_is_silent_for_unknown_events_and_outside_checkouts() {
    let h = Harness::start().await;
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    h.ok(&repo, &["init"]);
    seed_notices(&repo);
    let outside = h.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    for event in ["Stop", "PreToolUse", ""] {
        assert!(h.hook_from(&repo, event, "{}").is_none(), "{event}");
    }
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "SessionEnd",
    ] {
        assert!(h.hook_from(&outside, event, "{}").is_none(), "{event}");
        // Malformed input falls back to the process cwd.
        assert!(
            h.hook_from(&outside, event, "not json").is_none(),
            "{event}"
        );
    }
    // Unknown events consumed nothing.
    assert!(!repo.join(".oak/watch/cursor").exists());
}
