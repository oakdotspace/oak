//! `oak watch` — keep a task branch live while an agent works on it.
//!
//! A per-checkout background daemon ([`daemon`]) merges main into the
//! branch whenever main moves (checkpointing dirty work first; conflicts are
//! ordinary `oak pull` conflicts), pushes every new local commit, and
//! heartbeats an "Active" state the web shows. The agent hears about what
//! it did through the Claude Code hook entrypoint, `oak watch hook <event>`
//! ([`hook`]), which reads the daemon's notices from `.oak/watch/` without
//! touching the network.
//!
//! Watch applies to regular checkouts linked to a remote. Mounts (which
//! have their own virtual-branch lifecycle) and git-backed repos are not
//! watched.

mod daemon;
pub mod hook;
mod sse;
pub mod state;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oak_core::{OakError, Result};
use serde::Serialize;

use crate::commands::config::{self, AgentSyncSettings};
use crate::output;
use state::{unix_now, DaemonLock, WatchPaths, WatchState};

/// How long `oak watch start` waits for the spawned daemon to take its lock.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `oak watch stop` waits for a graceful exit (the daemon sends its
/// idle heartbeat, bounded at 5s, on the way out).
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// A checkout watch can run in.
pub(crate) struct WatchTarget {
    pub work_tree: PathBuf,
    pub oak_dir: PathBuf,
    pub paths: WatchPaths,
}

/// Resolve the regular, SQLite-backed checkout containing `path`. Mounts
/// and git-backed repos are refused with an explanation.
pub(crate) fn target(path: &Path) -> Result<WatchTarget> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if crate::commands::mount::mount_dest_for(path)
        .ok()
        .flatten()
        .is_some()
    {
        return Err(OakError::InvalidArgument(
            "oak watch runs in regular checkouts; a mount publishes through `oak mount finish` / `oak finish` instead"
                .to_string(),
        ));
    }
    let ctx = crate::resolve::resolve(path)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(OakError::InvalidArgument(
            "oak watch is not supported for git-backed repositories".to_string(),
        ));
    }
    Ok(WatchTarget {
        paths: WatchPaths::new(&ctx.oak_dir),
        work_tree: ctx.work_tree,
        oak_dir: ctx.oak_dir,
    })
}

#[derive(Serialize)]
pub struct WatchStatusJson {
    pub schema_version: u32,
    pub kind: &'static str,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub branch: Option<String>,
    pub repository_root: String,
    pub settings: AgentSyncSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_main_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pull_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pull_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pushed_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push_error: Option<String>,
    /// A parent-sync conflict watch produced that still awaits resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<state::PendingConflict>,
    pub sync_in_progress: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub unseen_notices: usize,
    pub log: String,
    pub recommended_next_commands: Vec<String>,
}

fn status_report(target: &WatchTarget) -> WatchStatusJson {
    let pid = target.paths.running_pid();
    let running = target.paths.daemon_running();
    let state = target.paths.load_state().unwrap_or_default();
    let repo = target
        .paths
        .dir
        .parent()
        .and_then(|oak_dir| oak_core::SqliteRepository::open(&oak_dir.join("oak.db")).ok());
    let branch = repo.as_ref().and_then(|repo| {
        oak_core::Repository::get_current_branch_name(repo)
            .ok()
            .flatten()
    });
    let settings = config::resolve_for_path(&target.work_tree);
    let sync_in_progress = target.oak_dir.join("SYNC_HEAD").exists();
    let conflict = state.conflict.clone().filter(|_| sync_in_progress);
    let mut next = Vec::new();
    if conflict.is_some() {
        next.push("oak conflict status".to_string());
        next.push("oak pull --continue".to_string());
    } else if !running && settings.enabled.value {
        next.push("oak watch start".to_string());
    }
    WatchStatusJson {
        schema_version: 1,
        kind: "watch_status",
        running,
        pid,
        branch,
        repository_root: target.work_tree.display().to_string(),
        settings,
        transport: state.transport.clone().filter(|_| running),
        heartbeat: state.heartbeat.clone(),
        last_main_head: state.last_main_head.clone(),
        last_pull_at: state.last_pull_at,
        last_pull_result: state.last_pull_result.clone(),
        last_pushed_head: state.last_pushed_head.clone(),
        last_push_at: state.last_push_at,
        last_push_error: state.last_push_error.clone(),
        conflict,
        sync_in_progress,
        last_error: state.last_error.clone(),
        stop_reason: state.stop_reason.clone().filter(|_| !running),
        unseen_notices: state.notices_after(target.paths.load_cursor()).count(),
        log: target.paths.log.display().to_string(),
        recommended_next_commands: next,
    }
}

/// `oak watch status [--json]`.
pub fn status(path: &Path, json: bool) -> Result<()> {
    let target = target(path)?;
    let report = status_report(&target);
    if json {
        return output::print_json(&report);
    }
    if report.running {
        output::success(&format!(
            "oak watch is running (pid {}) on branch '{}' via {}",
            report.pid.map_or("?".to_string(), |pid| pid.to_string()),
            report.branch.as_deref().unwrap_or("?"),
            report.transport.as_deref().unwrap_or("poll")
        ));
    } else {
        output::info(&format!(
            "oak watch is not running{}",
            report
                .stop_reason
                .as_deref()
                .map_or(String::new(), |reason| format!(" (last stop: {reason})"))
        ));
    }
    for (name, setting) in [
        ("watch.enabled", report.settings.enabled),
        ("watch.auto_pull", report.settings.auto_pull),
        ("watch.auto_push", report.settings.auto_push),
    ] {
        output::item(&format!(
            "{name} = {} ({})",
            setting.value,
            setting.source.describe()
        ));
    }
    if let Some(result) = &report.last_pull_result {
        output::item(&format!("last pull: {result}"));
    }
    if let Some(head) = &report.last_pushed_head {
        output::item(&format!(
            "last pushed head: {}",
            &head[..head.len().min(12)]
        ));
    }
    if let Some(error) = &report.last_push_error {
        output::item(&format!("last push error: {error}"));
    }
    if let Some(conflict) = &report.conflict {
        output::warning(&format!(
            "conflict pending in {}: resolve, then `oak pull --continue`",
            if conflict.paths.is_empty() {
                "the working tree".to_string()
            } else {
                conflict.paths.join(", ")
            }
        ));
    }
    output::item(&format!("log: {}", report.log));
    Ok(())
}

#[derive(Serialize)]
pub struct WatchStartJson {
    pub schema_version: u32,
    pub kind: &'static str,
    pub running: bool,
    pub already_running: bool,
    pub pid: Option<u32>,
    pub branch: String,
    pub log: String,
}

/// Preflight shared by every start path: a linked checkout on an open task
/// branch with watch enabled. Returns the branch.
fn start_preflight(target: &WatchTarget) -> Result<String> {
    let repo = oak_core::SqliteRepository::open(&target.oak_dir.join("oak.db"))?;
    daemon::load_remote_for(&repo)?;
    let branch = daemon::current_task_branch(&repo)?.ok_or_else(|| {
        OakError::InvalidArgument(
            "oak watch needs an open task branch; start one with `oak start \"<summary>\"`"
                .to_string(),
        )
    })?;
    let settings = config::resolve_for_path(&target.work_tree);
    if !settings.enabled.value {
        return Err(OakError::InvalidArgument(format!(
            "oak watch is disabled by the {} (watch.enabled = false). Enable it with `oak config set watch.enabled true`{}.",
            settings.enabled.source.describe(),
            if settings.enabled.source == config::Source::Env {
                " after unsetting OAK_WATCH"
            } else {
                ""
            }
        )));
    }
    Ok(branch)
}

/// Start the detached daemon for `path`'s checkout unless one is running.
/// `wait` blocks (briefly) until it holds its lock; the hook path passes
/// `false` to stay fast.
pub(crate) fn ensure_running(target: &WatchTarget, wait: bool) -> Result<WatchStartJson> {
    let branch = start_preflight(target)?;
    if let Some(pid) = target.paths.running_pid() {
        return Ok(WatchStartJson {
            schema_version: 1,
            kind: "watch_start",
            running: true,
            already_running: true,
            pid: Some(pid),
            branch,
            log: target.paths.log.display().to_string(),
        });
    }
    std::fs::create_dir_all(&target.paths.dir)?;
    target.paths.rotate_log();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target.paths.log)?;
    let log_err = log.try_clone()?;
    let exe = std::env::current_exe().map_err(OakError::Io)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["watch", "start", "--foreground"])
        .current_dir(&target.work_tree)
        .env("OAK_PROGRESS", "never")
        .env("NO_COLOR", "1")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    detach(&mut cmd);
    let mut child = cmd.spawn().map_err(OakError::Io)?;
    let child_pid = child.id();
    if wait {
        let deadline = Instant::now() + SPAWN_READY_TIMEOUT;
        loop {
            if target.paths.running_pid() == Some(child_pid) {
                break;
            }
            if let Some(status) = child.try_wait().map_err(OakError::Io)? {
                let log = std::fs::read_to_string(&target.paths.log).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(10).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                return Err(OakError::Server(format!(
                    "oak watch exited ({status}) before it started:\n{}",
                    tail.join("\n")
                )));
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(WatchStartJson {
        schema_version: 1,
        kind: "watch_start",
        running: true,
        already_running: false,
        pid: Some(child_pid),
        branch,
        log: target.paths.log.display().to_string(),
    })
}

/// `oak watch [start] [--foreground] [--json]`.
pub async fn start(path: &Path, foreground: bool, json: bool) -> Result<()> {
    let target = target(path)?;
    if foreground {
        let branch = start_preflight(&target)?;
        let Some(_lock) = DaemonLock::acquire(&target.paths)? else {
            return Err(OakError::InvalidArgument(format!(
                "oak watch is already running for this checkout (pid {}); see `oak watch status`",
                target
                    .paths
                    .load_pid()
                    .map_or("?".to_string(), |pid| pid.to_string())
            )));
        };
        daemon::log(&format!("watching branch '{branch}' in the foreground"));
        return daemon::run(&target.work_tree, &target.oak_dir).await;
    }
    let report = ensure_running(&target, true)?;
    if json {
        return output::print_json(&report);
    }
    if report.already_running {
        output::success(&format!(
            "oak watch is already running (pid {}) on branch '{}'",
            report.pid.unwrap_or_default(),
            report.branch
        ));
    } else {
        output::success(&format!(
            "oak watch started (pid {}) on branch '{}': main is merged in as it moves and new commits are pushed",
            report.pid.unwrap_or_default(),
            report.branch
        ));
        output::item(&format!("log: {}", report.log));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct WatchStopJson {
    pub schema_version: u32,
    pub kind: &'static str,
    pub was_running: bool,
    pub stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Whether this command itself told the server the branch is idle (the
    /// daemon does so on a graceful exit; this is the fallback).
    pub idle_sent: bool,
}

/// Stop the daemon. `wait` waits for a graceful exit (and falls back to a
/// hard kill + our own idle heartbeat); `send_idle` sends idle when no
/// daemon was running (a crashed one leaves the branch "active" until its
/// TTL). The hook passes neither, to stay fast and offline.
pub(crate) async fn stop_target(
    target: &WatchTarget,
    wait: bool,
    send_idle: bool,
) -> WatchStopJson {
    let pid = target.paths.running_pid();
    let mut report = WatchStopJson {
        schema_version: 1,
        kind: "watch_stop",
        was_running: pid.is_some(),
        stopped: pid.is_none(),
        pid,
        idle_sent: false,
    };
    let mut need_idle = send_idle && pid.is_none() && target.paths.state.exists();
    if let Some(pid) = pid {
        terminate(pid, false);
        if wait {
            let deadline = Instant::now() + STOP_TIMEOUT;
            while target.paths.daemon_running() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if target.paths.daemon_running() {
                terminate(pid, true);
                tokio::time::sleep(Duration::from_millis(200)).await;
                need_idle = true;
                // A killed daemon cannot record its own exit.
                let mut state = target.paths.load_state().unwrap_or_default();
                state.pid = None;
                state.transport = None;
                state.stopped_at = Some(unix_now());
                state.stop_reason = Some("killed".to_string());
                let _ = target.paths.save_state(&state);
            }
            report.stopped = !target.paths.daemon_running();
        } else {
            report.stopped = true;
        }
    }
    if need_idle {
        report.idle_sent = send_idle_heartbeat(target).await;
    }
    report
}

async fn send_idle_heartbeat(target: &WatchTarget) -> bool {
    let Ok(repo) = oak_core::SqliteRepository::open(&target.oak_dir.join("oak.db")) else {
        return false;
    };
    let Ok((remote, owner, name)) = daemon::load_remote_for(&repo) else {
        return false;
    };
    let Some(branch) = oak_core::Repository::get_current_branch_name(&repo)
        .ok()
        .flatten()
    else {
        return false;
    };
    let api_key = crate::commands::credentials::effective_token_for_repository(&remote, &repo);
    daemon::post_activity(
        &crate::http::api_client(),
        &remote,
        &format!("{owner}/{name}"),
        &branch,
        api_key.as_deref(),
        "idle",
    )
    .await
    .is_ok_and(|status| status.is_success())
}

/// `oak watch stop [--json]`.
pub async fn stop(path: &Path, json: bool) -> Result<()> {
    let target = target(path)?;
    let report = stop_target(&target, true, true).await;
    if json {
        return output::print_json(&report);
    }
    match (report.was_running, report.stopped) {
        (true, true) => output::success(&format!(
            "Stopped oak watch (pid {})",
            report.pid.unwrap_or_default()
        )),
        (true, false) => output::warning(&format!(
            "oak watch (pid {}) did not stop",
            report.pid.unwrap_or_default()
        )),
        (false, _) => output::info("oak watch was not running"),
    }
    Ok(())
}

/// `oak finish` ends the task: stop watch (so its auto-push cannot race the
/// finish publication) and mark the branch idle. Best-effort and silent.
pub async fn stop_for_finish(path: &Path) {
    if let Ok(target) = target(path) {
        if target.paths.dir.exists() {
            let report = stop_target(&target, true, true).await;
            if report.was_running {
                output::vlog("finish: stopped oak watch");
            }
        }
    }
}

/// The settings and run state `oak environment --json` reports for the
/// checkout containing `path` (local reads only).
#[derive(Serialize)]
pub struct WatchEnvironment {
    pub settings: AgentSyncSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    pub global_config: Option<String>,
    pub precedence: [&'static str; 5],
}

pub fn environment(path: &Path) -> WatchEnvironment {
    WatchEnvironment {
        settings: config::resolve_for_path(path),
        running: target(path).ok().map(|t| t.paths.daemon_running()),
        global_config: config::global_config_path().map(|p| p.display().to_string()),
        precedence: ["env", "repo", "global", "server", "default"],
    }
}

/// Unread notices for `state` past the hook cursor.
pub(crate) fn unseen(state: &WatchState, cursor: u64) -> Vec<state::Notice> {
    state.notices_after(cursor).cloned().collect()
}

#[cfg(unix)]
fn terminate(pid: u32, force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

#[cfg(not(unix))]
fn terminate(pid: u32, _force: bool) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Run the daemon in its own session so it outlives the agent's shell and
/// does not catch the SIGHUP when that process group goes away.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and the only thing run in the
    // forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut std::process::Command) {}
