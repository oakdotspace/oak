//! `oak watch` behaviour that needs no server: where watch refuses to run,
//! and what the agent hooks print from notices already on disk.
//!
//! The end-to-end watch tests (start, the daemon syncing with main, the event
//! stream, heartbeats, shared work) run against the real
//! server.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Harness {
    fn start() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self {
            _temp: temp,
            root,
            home,
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

#[test]
fn watch_refuses_mounts_and_git_backed_repositories() {
    let h = Harness::start();

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

#[test]
fn watch_start_needs_a_linked_checkout() {
    let h = Harness::start();

    // Never published: no remote to sync with.
    let local = h.root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    h.ok(&local, &["init"]);
    let (code, text) = h.fails(&local, &["watch", "start"], &[]);
    assert_ne!(code, Some(0), "{text}");
    assert!(text.contains("not linked to an Oak remote"), "{text}");
    assert!(!h.watch_status(&local)["running"].as_bool().unwrap());
}

#[test]
fn hook_user_prompt_submit_delivers_notices_once_and_repeats_open_conflicts() {
    let h = Harness::start();
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

#[test]
fn hook_post_tool_use_delivers_notices_without_conflict_reminders() {
    let h = Harness::start();
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

#[test]
fn hook_is_silent_for_unknown_events_and_outside_checkouts() {
    let h = Harness::start();
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
