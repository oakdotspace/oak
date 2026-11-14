//! The `oak watch` daemon loop (one per checkout).
//!
//! Every tick (1s) it decides, from cheap local reads, whether there is work:
//!
//! - **Exit** when the checkout is gone, the current branch is `main`/none
//!   or closed, watch was disabled, or a signal asked it to stop.
//! - **Pull** when the event stream reported a push/merge to main (or lag,
//!   or a reconnect), or the poll interval elapsed: if remote main (or the
//!   branch's own remote head) is not yet in the branch's history, it
//!   checkpoints any dirty work with `oak commit --no-verify` and runs the
//!   ordinary `oak pull` in-process. A conflict leaves the usual
//!   `.oak/SYNC_HEAD` and pauses pulling until the agent resolves it.
//! - **Push** every 5s when the branch head differs from the last pushed
//!   head.
//! - **Heartbeat** `POST .../activity` every 60s (ttl 120s); idle on exit.
//! - **Share** with `watch.share_working` on: scan for uncommitted text files
//!   every 2s and `PUT .../working` when they changed, or every 60s as a
//!   keepalive. Cleared on exit, on leaving the branch, and when the setting
//!   goes off. A slow scan stretches its own interval, so a huge tree isn't
//!   rescanned constantly. See [`super::working`].
//! - **Idle exit** after 4h (`OAK_WATCH_IDLE_SECS`, 0 to disable) with neither
//!   a hook from an agent session nor a commit, so an agent that died without
//!   `SessionEnd` does not leave a branch syncing and Active forever. The hook
//!   restarts it when an agent comes back.
//!
//! It never runs concurrently with the agent's own write commands: before a
//! checkpoint, pull or push it skips the tick while `.oak/wdlock` is held or
//! a merge/sync is in progress, and a `RepoLocked` from the command itself
//! (a race) just retries a couple of seconds later. Network failures back
//! off exponentially; endpoints an older server lacks (404) degrade to
//! polling / no heartbeat / no server preference.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oak_core::{BranchStatus, Hash, MetadataKey, OakError, Repository, Result, SqliteRepository};
use tokio::sync::mpsc;

use super::sse::{self, StreamSignal};
use super::state::{unix_now, PendingConflict, WatchPaths, WatchState, IDLE_STOP_REASON};
use super::working;
use crate::commands::config::{self, AgentSyncSettings};

const TICK: Duration = Duration::from_secs(1);
const PUSH_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const HEARTBEAT_TTL_SECS: u64 = 120;
const WORKING_INTERVAL: Duration = Duration::from_secs(2);
/// The server drops a snapshot after 120s without a resend.
const WORKING_KEEPALIVE: Duration = Duration::from_secs(60);
/// A scan may take at most this share of the daemon's time: `1 / N`.
const WORKING_SCAN_SHARE: u32 = 10;
/// Wait for `maybe_push` to publish the head a snapshot refers to.
const WORKING_PUSH_WAIT: Duration = Duration::from_secs(5);
/// Back off after the server refuses a snapshot's content.
const WORKING_REFUSED_WAIT: Duration = Duration::from_secs(60);
/// Poll cadence without the event stream (older servers, `oak serve`).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Safety poll while the stream is connected, for missed events.
const STREAM_POLL_INTERVAL: Duration = Duration::from_secs(300);
/// How long the daemon outlives the last sign of an agent.
pub(crate) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
/// Retry delay when the workdir lock or an operation blocks us.
const LOCK_RETRY: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How long a push the server deterministically refused for lack of a
/// capability is left alone while the branch head does not change.
const BLOCKED_PUSH_RECHECK: Duration = Duration::from_secs(30 * 60);

fn blocked_push_recheck() -> Duration {
    std::env::var("OAK_WATCH_BLOCKED_PUSH_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(BLOCKED_PUSH_RECHECK)
}
/// How long an unsupported endpoint is left alone before trying again.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(600);
const PREFS_REFRESH: Duration = Duration::from_secs(600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Paths listed in a "main merged" notice before eliding the rest.
const NOTICE_PATH_LIMIT: usize = 10;

fn poll_interval() -> Duration {
    std::env::var("OAK_WATCH_POLL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
}

/// `None` when `OAK_WATCH_IDLE_SECS=0` turned the idle exit off.
pub(crate) fn idle_timeout() -> Option<Duration> {
    match std::env::var("OAK_WATCH_IDLE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(DEFAULT_IDLE_TIMEOUT),
    }
}

pub(crate) fn log(message: &str) {
    eprintln!("[{}] {message}", chrono::Utc::now().to_rfc3339());
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(10)]
}

/// Why the loop ended.
#[derive(Debug)]
enum Exit {
    Signal,
    CheckoutGone,
    OnMain,
    BranchClosed(String),
    Disabled(&'static str),
    BranchMerged(String),
    Idle(Duration),
}

impl Exit {
    fn reason(&self) -> String {
        match self {
            Exit::Signal => "stopped".to_string(),
            Exit::CheckoutGone => "checkout_gone".to_string(),
            Exit::OnMain => "no_task_branch".to_string(),
            Exit::BranchClosed(_) => "branch_closed".to_string(),
            Exit::Disabled(source) => format!("disabled_by_{source}"),
            Exit::BranchMerged(_) => "branch_merged".to_string(),
            Exit::Idle(_) => IDLE_STOP_REASON.to_string(),
        }
    }
}

/// Exponential backoff with a floor and `MAX_BACKOFF` cap.
struct Backoff {
    floor: Duration,
    current: Duration,
}

impl Backoff {
    fn new(floor: Duration) -> Self {
        Self {
            floor,
            current: floor,
        }
    }
    fn fail(&mut self) -> Duration {
        let wait = self.current;
        self.current = (self.current * 2).min(MAX_BACKOFF);
        wait
    }
    fn reset(&mut self) {
        self.current = self.floor;
    }
}

#[derive(Clone)]
struct Remote {
    url: String,
    owner: String,
    name: String,
    api_key: Option<String>,
}

impl Remote {
    fn repo_path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

struct Daemon {
    work_tree: PathBuf,
    oak_dir: PathBuf,
    paths: WatchPaths,
    remote: Remote,
    client: reqwest::Client,
    branch: String,
    state: WatchState,
    settings: AgentSyncSettings,

    stream_rx: mpsc::UnboundedReceiver<StreamSignal>,
    stream_task: tokio::task::JoinHandle<()>,
    stream_connected: bool,

    check_requested: bool,
    next_check: Instant,
    check_backoff: Backoff,
    pull_failures: u32,
    next_push: Instant,
    push_backoff: Backoff,
    next_heartbeat: Instant,
    heartbeat_backoff: Backoff,
    heartbeat_ok_once: bool,
    next_prefs: Instant,

    next_working: Instant,
    working_backoff: Backoff,
    working_label: String,
    /// What the server holds for this checkout, when it holds anything.
    working_sent: Option<SentWorking>,

    idle_timeout: Option<Duration>,
    last_activity: u64,
    seen_head: Option<Hash>,
}

/// The snapshot the server last accepted.
struct SentWorking {
    branch: String,
    head: String,
    fingerprint: String,
    at: Instant,
}

/// Run the daemon in this process until it exits. The caller has already
/// taken the singleton [`super::state::DaemonLock`].
pub(crate) async fn run(work_tree: &Path, oak_dir: &Path) -> Result<()> {
    // Listen for stop signals before anything else: until a handler is
    // registered, SIGTERM (`oak watch stop`, the SessionEnd hook) kills the
    // daemon outright, without an idle heartbeat or a recorded stop reason.
    let shutdown = shutdown_signal();
    let paths = WatchPaths::new(oak_dir);
    let repo = open_repo(oak_dir)?;
    let remote = load_remote(repo.as_ref())?;
    let branch = current_task_branch(repo.as_ref())?
        .ok_or_else(|| OakError::InvalidArgument("no task branch to watch".to_string()))?;
    let seen_head = repo.get_branch_head(&branch).ok().flatten();
    drop(repo);

    let mut state = paths.load_state().unwrap_or_default();
    let carried_branch = state.branch.clone();
    state.schema_version = 1;
    state.pid = Some(std::process::id());
    state.started_at = Some(unix_now());
    state.stopped_at = None;
    state.stop_reason = None;
    state.transport = Some("poll".to_string());
    if carried_branch.as_deref() != Some(branch.as_str()) {
        reset_branch_state(&mut state);
    }
    state.branch = Some(branch.clone());
    // A new daemon may be a newer CLI: ask the server once more.
    state.push_blocked = None;

    let client = crate::http::api_client();
    let (stream_tx, stream_rx) = mpsc::unbounded_channel();
    let stream_task = spawn_stream(&client, &remote, &branch, stream_tx);
    let now = Instant::now();
    let mut daemon = Daemon {
        work_tree: work_tree.to_path_buf(),
        oak_dir: oak_dir.to_path_buf(),
        paths,
        settings: resolve_settings(oak_dir, &remote.url),
        remote,
        client,
        branch,
        state,
        stream_rx,
        stream_task,
        stream_connected: false,
        check_requested: true,
        next_check: now,
        check_backoff: Backoff::new(Duration::from_secs(5)),
        pull_failures: 0,
        next_push: now,
        push_backoff: Backoff::new(Duration::from_secs(10)),
        next_heartbeat: now,
        heartbeat_backoff: Backoff::new(Duration::from_secs(30)),
        heartbeat_ok_once: false,
        next_prefs: now,
        next_working: now,
        working_backoff: Backoff::new(Duration::from_secs(5)),
        working_label: working::client_label(work_tree),
        working_sent: None,
        idle_timeout: idle_timeout(),
        last_activity: unix_now(),
        seen_head,
    };
    log(&format!(
        "oak watch started for {} on branch '{}' ({})",
        daemon.work_tree.display(),
        daemon.branch,
        daemon.remote.url
    ));
    daemon.save();

    let exit = daemon.run_loop(shutdown).await;
    log(&format!("oak watch stopping: {exit:?}"));
    daemon.stream_task.abort();
    daemon.clear_working().await;
    if daemon.heartbeat_ok_once {
        let _ = send_activity(&daemon.client, &daemon.remote, &daemon.branch, "idle").await;
    }
    match &exit {
        Exit::BranchMerged(branch) => daemon.state.push_notice(
            "stopped",
            format!("Branch '{branch}' was merged into main; oak watch stopped. Start the next task with `oak start \"<summary>\"`."),
        ),
        Exit::BranchClosed(branch) => daemon.state.push_notice(
            "stopped",
            format!("Branch '{branch}' is closed; oak watch stopped."),
        ),
        Exit::Idle(after) => daemon.state.push_notice(
            "stopped",
            format!(
                "oak watch stopped after {}s without agent activity, so this branch was not synced meanwhile. It restarts with the next hook, or run `oak watch start`.",
                after.as_secs()
            ),
        ),
        _ => {}
    }
    daemon.state.transport = None;
    daemon.state.stopped_at = Some(unix_now());
    daemon.state.stop_reason = Some(exit.reason());
    daemon.state.pid = None;
    daemon.save();
    Ok(())
}

fn reset_branch_state(state: &mut WatchState) {
    state.last_pushed_head = None;
    state.last_push_error = None;
    state.push_blocked = None;
    state.working = None;
    state.working_files = None;
    state.working_skipped = None;
    state.conflict = None;
    state.skip_main_head = None;
    state.last_pull_result = None;
}

fn open_repo(oak_dir: &Path) -> Result<Box<dyn Repository>> {
    Ok(Box::new(SqliteRepository::open(&oak_dir.join("oak.db"))?))
}

/// The remote this checkout publishes to, or a `Config` error when it is
/// not linked yet (watch has nothing to sync with).
pub(crate) fn load_remote_for(repo: &dyn Repository) -> Result<(String, String, String)> {
    let url = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .and_then(|url| crate::commands::push::normalize_remote_url(&url));
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?;
    let name = repo.get_metadata(MetadataKey::RepoName)?;
    let url = crate::commands::push::env_remote_override().or(url);
    match (url, owner, name) {
        (Some(url), Some(owner), Some(name)) => Ok((url, owner, name)),
        _ => Err(OakError::RepoNotLinked(format!(
            "this checkout is not linked to an Oak remote yet, so there is nothing to watch. Publish it first with `{}`.",
            crate::commands::push::PUSH_REPO_PLACEHOLDER_COMMAND
        ))),
    }
}

fn load_remote(repo: &dyn Repository) -> Result<Remote> {
    let (url, owner, name) = load_remote_for(repo)?;
    let api_key = crate::commands::credentials::effective_token_for_repository(&url, repo);
    Ok(Remote {
        url,
        owner,
        name,
        api_key,
    })
}

/// The current branch when it is an open task branch (not `main`, not
/// closed, not detached).
pub(crate) fn current_task_branch(repo: &dyn Repository) -> Result<Option<String>> {
    let Some(name) = repo.get_current_branch_name()?.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name == oak_core::DEFAULT_BRANCH {
        return Ok(None);
    }
    match repo.get_branch(&name)? {
        Some(branch) if branch.status != BranchStatus::Closed => Ok(Some(name)),
        _ => Ok(None),
    }
}

fn resolve_settings(oak_dir: &Path, remote: &str) -> AgentSyncSettings {
    let repo = open_repo(oak_dir).ok();
    config::Layers::load(repo.as_deref(), Some(remote)).resolve()
}

fn spawn_stream(
    client: &reqwest::Client,
    remote: &Remote,
    branch: &str,
    tx: mpsc::UnboundedSender<StreamSignal>,
) -> tokio::task::JoinHandle<()> {
    let url = sse::events_url(
        &remote.url,
        &remote.repo_path(),
        &[oak_core::DEFAULT_BRANCH, branch],
    );
    tokio::spawn(sse::run(
        client.clone(),
        url,
        remote.api_key.clone(),
        branch.to_string(),
        tx,
    ))
}

impl Daemon {
    fn save(&self) {
        if let Err(error) = self.paths.save_state(&self.state) {
            log(&format!("could not save watch state: {error}"));
        }
    }

    fn notice(&mut self, kind: &str, message: String) {
        log(&format!("notice ({kind}): {message}"));
        self.state.push_notice(kind, message);
    }

    async fn run_loop(&mut self, shutdown: impl std::future::Future<Output = ()>) -> Exit {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => return Exit::Signal,
                Some(signal) = self.stream_rx.recv() => self.on_stream(signal),
                _ = tick.tick() => {}
            }
            if let Some(exit) = self.check_checkout() {
                return exit;
            }
            let now = Instant::now();
            if now >= self.next_prefs {
                self.refresh_server_preferences().await;
                if !self.settings.enabled.value {
                    return Exit::Disabled(source_tag(self.settings.enabled.source));
                }
            }
            if self.check_requested || now >= self.next_check {
                if let Some(exit) = self.check_and_pull().await {
                    self.save();
                    return exit;
                }
                self.save();
            }
            if Instant::now() >= self.next_push {
                self.maybe_push().await;
                self.save();
            }
            if Instant::now() >= self.next_heartbeat {
                self.heartbeat().await;
                self.save();
            }
            if Instant::now() >= self.next_working {
                self.maybe_share_working().await;
                self.save();
            }
        }
    }

    fn on_stream(&mut self, signal: StreamSignal) {
        match signal {
            StreamSignal::Connected => {
                log("event stream connected");
                self.stream_connected = true;
                self.state.transport = Some("sse".to_string());
                // Events may have been missed while disconnected.
                self.check_requested = true;
            }
            StreamSignal::Disconnected(reason) => {
                if self.stream_connected {
                    log(&format!("event stream disconnected: {reason}"));
                }
                self.stream_connected = false;
                self.state.transport = Some("poll".to_string());
            }
            StreamSignal::Unsupported => {
                log("server has no event stream; polling");
                self.stream_connected = false;
                self.state.transport = Some("poll".to_string());
            }
            StreamSignal::MainMoved | StreamSignal::Lag | StreamSignal::BranchPushed => {
                self.check_requested = true;
            }
            StreamSignal::BranchClosed => {
                // Confirmed (and acted on) by the next check's branch read.
                self.check_requested = true;
            }
        }
        self.save();
    }

    /// Local exit conditions and live settings, from cheap reads.
    fn check_checkout(&mut self) -> Option<Exit> {
        if !self.oak_dir.join("oak.db").exists() || !self.work_tree.exists() {
            return Some(Exit::CheckoutGone);
        }
        let repo = open_repo(&self.oak_dir).ok()?;
        let current = repo.get_current_branch_name().ok().flatten();
        match current_task_branch(repo.as_ref()) {
            Ok(Some(branch)) if branch != self.branch => {
                // Leaving a branch that is now closed or gone (`oak merge`
                // removes it locally and rotates to a fresh personal branch)
                // ends the task: stop rather than follow the rotation.
                if let Ok(old) = repo.get_branch(&self.branch) {
                    if old.is_none_or(|old| old.status == BranchStatus::Closed) {
                        return Some(Exit::BranchClosed(self.branch.clone()));
                    }
                }
                // The agent switched tasks (`oak start`, `oak switch`): follow
                // the new branch with fresh per-branch state.
                log(&format!("branch changed: '{}' -> '{branch}'", self.branch));
                let old = std::mem::replace(&mut self.branch, branch);
                if self.heartbeat_ok_once {
                    let client = self.client.clone();
                    let remote = self.remote.clone();
                    tokio::spawn(async move {
                        let _ = send_activity(&client, &remote, &old, "idle").await;
                    });
                }
                if let Some(sent) = self.working_sent.take() {
                    let client = self.client.clone();
                    let remote = self.remote.clone();
                    let label = self.working_label.clone();
                    tokio::spawn(async move {
                        let _ = clear_snapshot(&client, &remote, &sent, &label).await;
                    });
                }
                reset_branch_state(&mut self.state);
                self.state.branch = Some(self.branch.clone());
                self.stream_task.abort();
                let (tx, rx) = mpsc::unbounded_channel();
                self.stream_rx = rx;
                self.stream_task = spawn_stream(&self.client, &self.remote, &self.branch, tx);
                self.stream_connected = false;
                self.state.transport = Some("poll".to_string());
                self.check_requested = true;
                self.next_push = Instant::now();
                self.next_heartbeat = Instant::now();
                self.next_working = Instant::now();
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return Some(match current {
                    Some(name) if name != oak_core::DEFAULT_BRANCH && !name.is_empty() => {
                        Exit::BranchClosed(name)
                    }
                    _ => Exit::OnMain,
                });
            }
            Err(_) => return None,
        }
        self.settings = config::Layers::load(Some(repo.as_ref()), Some(&self.remote.url)).resolve();
        if !self.settings.enabled.value {
            return Some(Exit::Disabled(source_tag(self.settings.enabled.source)));
        }
        let head = repo.get_branch_head(&self.branch).ok().flatten();
        self.check_idle(head)
    }

    /// Exit once nothing has shown an agent is still here for the timeout: no
    /// session hook, and no head movement other than the daemon's own.
    fn check_idle(&mut self, head: Option<Hash>) -> Option<Exit> {
        let timeout = self.idle_timeout?;
        let now = unix_now();
        if head != self.seen_head {
            self.seen_head = head;
            self.last_activity = now;
        }
        if let Some(at) = self.paths.last_session_activity() {
            self.last_activity = self.last_activity.max(at);
        }
        (now.saturating_sub(self.last_activity) >= timeout.as_secs()).then_some(Exit::Idle(timeout))
    }

    async fn refresh_server_preferences(&mut self) {
        self.next_prefs = Instant::now() + PREFS_REFRESH;
        match config::fetch_server_preferences(
            &self.remote.url,
            self.remote.api_key.as_deref(),
            REQUEST_TIMEOUT,
        )
        .await
        {
            Ok(_) => self.settings = resolve_settings(&self.oak_dir, &self.remote.url),
            Err(error) => log(&format!("could not read server preferences: {error}")),
        }
    }

    fn operation_in_progress(&self) -> bool {
        self.oak_dir.join("SYNC_HEAD").exists() || self.oak_dir.join("MERGE_HEAD").exists()
    }

    /// Whether another process holds the workdir lock right now.
    fn workdir_locked(&self) -> bool {
        let lock = self.oak_dir.join("wdlock");
        lock.exists()
            && !crate::workdir_lock::reap_if_owner_dead(&lock).unwrap_or(false)
            && lock.exists()
    }

    fn defer_check(&mut self, wait: Duration) {
        self.check_requested = false;
        self.next_check = Instant::now() + wait;
    }

    fn scheduled_check(&mut self) {
        let interval = if self.stream_connected {
            STREAM_POLL_INTERVAL
        } else {
            poll_interval()
        };
        self.defer_check(interval);
    }

    /// Settle a conflict the agent has finished with (SYNC_HEAD gone): either
    /// resolved (`oak pull --continue`) or backed out (`--abort`).
    fn settle_conflict(&mut self, repo: &dyn Repository) {
        let Some(conflict) = self.state.conflict.take() else {
            return;
        };
        let head = repo.get_branch_head(&self.branch).ok().flatten();
        let merged = match (head, conflict.main_head.as_deref()) {
            (Some(head), Some(main)) => {
                crate::commands::sync::local_history_contains(repo, &head, &Hash(main.to_string()))
                    .unwrap_or(false)
            }
            _ => false,
        };
        if merged {
            log("conflict resolved; auto-pull resumed");
            self.state.skip_main_head = None;
        } else {
            self.state.skip_main_head = conflict.main_head.clone();
            self.notice(
                "pull_aborted",
                "The merge of main was aborted, so this branch is still behind main. oak watch will not retry that merge until main moves again; run `oak pull` when you want to merge it yourself.".to_string(),
            );
        }
    }

    /// Check whether main (or this branch's remote head) moved past the
    /// branch, and if so checkpoint + pull.
    async fn check_and_pull(&mut self) -> Option<Exit> {
        if self.operation_in_progress() {
            // Paused: a merge/sync (ours or the agent's) awaits resolution.
            self.defer_check(LOCK_RETRY);
            return None;
        }
        let repo = open_repo(&self.oak_dir).ok()?;
        self.settle_conflict(repo.as_ref());

        let main_head = match self.remote_head(oak_core::DEFAULT_BRANCH).await {
            Ok(head) => head,
            Err(error) => {
                let wait = self.check_backoff.fail();
                self.state.last_error = Some(error.to_string());
                log(&format!(
                    "main check failed ({error}); retrying in {wait:?}"
                ));
                self.defer_check(wait);
                return None;
            }
        };
        let branch_remote = self.remote_branch(&self.branch.clone()).await;
        self.check_backoff.reset();
        self.state.last_check_at = Some(unix_now());
        self.state.last_main_head = main_head.as_ref().map(|h| h.to_string());
        // The poll is a verified read of the remote main head: record it so
        // every other command sees the same observation (and sees that local
        // main is behind when the head moved).
        crate::remote_head::record_seen(repo.as_ref(), main_head.as_ref());

        if let Ok(Some(status)) = &branch_remote.status {
            if status == "closed" || status == "merged" {
                return Some(if status == "merged" {
                    Exit::BranchMerged(self.branch.clone())
                } else {
                    Exit::BranchClosed(self.branch.clone())
                });
            }
        }

        if !self.settings.auto_pull.value {
            self.scheduled_check();
            return None;
        }

        let local_head = repo.get_branch_head(&self.branch).ok().flatten();
        let contains = |target: &Hash| match &local_head {
            Some(head) => {
                crate::commands::sync::local_history_contains(repo.as_ref(), head, target)
                    .unwrap_or(false)
            }
            None => false,
        };
        let main_behind = main_head.as_ref().is_some_and(|main| {
            !contains(main) && self.state.skip_main_head.as_deref() != Some(main.0.as_str())
        });
        let branch_behind = branch_remote
            .head
            .as_ref()
            .is_some_and(|remote| !contains(remote));
        drop(repo);
        if !main_behind && !branch_behind {
            self.scheduled_check();
            return None;
        }
        if self.workdir_locked() {
            self.defer_check(LOCK_RETRY);
            return None;
        }
        let exit = self.pull(main_head).await;
        // The daemon's own checkpoint and merge are not agent activity.
        self.seen_head = open_repo(&self.oak_dir)
            .ok()
            .and_then(|repo| repo.get_branch_head(&self.branch).ok().flatten());
        exit
    }

    async fn pull(&mut self, main_head: Option<Hash>) -> Option<Exit> {
        // Checkpoint dirty work first so the merge never has to carry
        // uncommitted edits. Hooks are skipped: this is an automatic
        // checkpoint, not the agent's commit, and a failing lint hook must
        // not wedge syncing.
        //
        // A pull killed mid-update (this daemon's or the agent's) left a
        // torn tree. Recover it here only when that leaves nothing for a
        // person to decide; otherwise keep the record, never checkpoint
        // over it, and say what to run.
        let work_tree = self.work_tree.clone();
        let recovery = tokio::task::spawn_blocking(move || {
            crate::worktree_update::recover_unattended(&work_tree)
        })
        .await
        .unwrap_or_else(|e| Err(OakError::Io(std::io::Error::other(e))));
        match recovery {
            Ok(Some(recovered)) => log(&recovered.message()),
            Ok(None) => {}
            Err(OakError::RepoLocked) => {
                self.defer_check(LOCK_RETRY);
                return None;
            }
            Err(error) => {
                let wait = self.check_backoff.fail();
                let message = error.to_string();
                // Once, not on every retry.
                if self.state.last_error.as_deref() != Some(message.as_str()) {
                    self.notice("interrupted_update", message.clone());
                }
                self.state.last_error = Some(message);
                self.defer_check(wait);
                return None;
            }
        }
        let work_tree = self.work_tree.clone();
        let commit = tokio::task::spawn_blocking(move || {
            crate::commands::commit::run_with_options(
                &work_tree,
                crate::commands::commit::CommitOptions {
                    no_verify: true,
                    quiet: true,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap_or_else(|e| Err(OakError::Io(std::io::Error::other(e))));
        match commit {
            Ok(()) => {}
            Err(OakError::RepoLocked) => {
                self.defer_check(LOCK_RETRY);
                return None;
            }
            Err(error) => {
                let wait = self.check_backoff.fail();
                self.notice(
                    "checkpoint_failed",
                    format!("oak watch could not checkpoint your changes before merging main ({error}); auto-pull will retry. Run `oak commit` yourself to see why."),
                );
                self.defer_check(wait);
                return None;
            }
        }

        let repo = open_repo(&self.oak_dir).ok()?;
        let head_before = repo.get_branch_head(&self.branch).ok().flatten();
        drop(repo);
        log(&format!(
            "pulling: main at {}",
            main_head
                .as_ref()
                .map_or("none".to_string(), |h| short(&h.0).to_string())
        ));
        let result = crate::commands::pull::run_for_watch(&self.work_tree).await;
        self.state.last_pull_at = Some(unix_now());
        match result {
            Err(OakError::RepoLocked) => {
                self.defer_check(LOCK_RETRY);
                None
            }
            Err(error) => {
                self.pull_failed(error.to_string());
                None
            }
            Ok((receipt, deferred)) => {
                use crate::commands::pull::ParentSyncState;
                self.state.last_pull_result = Some(receipt.convergence.to_string());
                if receipt.parent_sync == ParentSyncState::ReconciledMergedBranch {
                    return Some(Exit::BranchMerged(self.branch.clone()));
                }
                if self.operation_in_progress() || receipt.parent_sync == ParentSyncState::Conflict
                {
                    self.record_conflict(main_head, &receipt);
                    return None;
                }
                if let Some(error) = deferred {
                    self.pull_failed(error.to_string());
                    return None;
                }
                self.pull_failures = 0;
                self.check_backoff.reset();
                self.announce_merge(main_head.as_ref(), head_before, &receipt);
                self.scheduled_check();
                // The sync commit is new local history: push it promptly.
                self.next_push = Instant::now();
                None
            }
        }
    }

    fn pull_failed(&mut self, error: String) {
        let wait = self.check_backoff.fail();
        self.pull_failures += 1;
        self.state.last_error = Some(error.clone());
        log(&format!("pull failed ({error}); retrying in {wait:?}"));
        // Tell the agent once per failure streak, not on every retry.
        if self.pull_failures == 1 {
            self.notice(
                "pull_failed",
                format!("oak watch could not merge the latest main into '{}': {error}. It will keep retrying; run `oak pull` to see the full error.", self.branch),
            );
        }
        self.defer_check(wait);
    }

    fn record_conflict(
        &mut self,
        main_head: Option<Hash>,
        receipt: &crate::commands::pull::PullJson,
    ) {
        let mut paths = receipt.conflict_paths.clone();
        if paths.is_empty() {
            paths = crate::work_state::conflict_paths_for_path(&self.work_tree);
        }
        let main = main_head.as_ref().map(|h| h.0.clone());
        let listed = if paths.is_empty() {
            String::new()
        } else {
            format!(" in: {}", paths.join(", "))
        };
        self.notice(
            "conflict",
            format!(
                "CONFLICT: oak watch merged the latest main ({}) into branch '{}' and hit conflicts{listed}. Resolve this now, before continuing your task: run `oak conflict status`, edit each conflicted file to remove the conflict markers, then run `oak pull --continue` (or `oak pull --abort` to back out). Auto-pull is paused until then.",
                main.as_deref().map_or("unknown", short),
                self.branch
            ),
        );
        self.state.conflict = Some(PendingConflict {
            main_head: main,
            paths,
            at: unix_now(),
        });
        self.defer_check(LOCK_RETRY);
    }

    fn announce_merge(
        &mut self,
        main_head: Option<&Hash>,
        head_before: Option<Hash>,
        receipt: &crate::commands::pull::PullJson,
    ) {
        let head_after = open_repo(&self.oak_dir)
            .ok()
            .and_then(|repo| repo.get_branch_head(&self.branch).ok().flatten());
        if head_after.is_none() || head_after == head_before {
            return;
        }
        let changed = open_repo(&self.oak_dir)
            .ok()
            .map(|repo| changed_paths(repo.as_ref(), head_before.as_ref(), head_after.as_ref()))
            .unwrap_or_default();
        let files = match changed.len() {
            0 => String::new(),
            n if n <= NOTICE_PATH_LIMIT => format!(" {n} file(s) changed: {}.", changed.join(", ")),
            n => format!(
                " {n} files changed, including: {}.",
                changed[..NOTICE_PATH_LIMIT].join(", ")
            ),
        };
        let what = match receipt.parent_sync {
            crate::commands::pull::ParentSyncState::UpToDate => "Pulled new commits on",
            _ => "Merged the latest main into",
        };
        self.notice(
            "main_merged",
            format!(
                "oak watch: {what} branch '{}' (main at {}).{files} Re-read those files before editing them.",
                self.branch,
                main_head.map_or("unknown", |h| short(&h.0))
            ),
        );
    }

    async fn maybe_push(&mut self) {
        self.next_push = Instant::now() + PUSH_INTERVAL;
        if !self.settings.auto_push.value || self.operation_in_progress() {
            return;
        }
        let Ok(repo) = open_repo(&self.oak_dir) else {
            return;
        };
        let Some(head) = repo.get_branch_head(&self.branch).ok().flatten() else {
            return;
        };
        drop(repo);
        let head = head.to_string();
        let receipt = crate::work_state::checkout_push_receipt(
            &self.oak_dir,
            &self.remote.url,
            &self.remote.owner,
            &self.remote.name,
            &self.branch,
        )
        .and_then(|(head, _)| head);
        if receipt.as_deref() == Some(head.as_str())
            || self.state.last_pushed_head.as_deref() == Some(head.as_str())
        {
            // Published some other way (a manual `oak push`): nothing is
            // blocked any more.
            if self.state.push_blocked.take().is_some() {
                self.state.last_push_error = None;
            }
            return;
        }
        if self
            .state
            .push_blocked
            .as_ref()
            .is_some_and(|blocked| blocked.head == head && unix_now() < blocked.next_retry_at)
        {
            return;
        }
        if self.workdir_locked() {
            return;
        }
        log(&format!("pushing '{}' at {}", self.branch, short(&head)));
        match crate::commands::push::run(&self.work_tree, None, false, None).await {
            Ok(()) => {
                self.push_backoff.reset();
                self.state.last_pushed_head = Some(head);
                self.state.last_push_at = Some(unix_now());
                self.state.last_push_error = None;
                self.state.push_blocked = None;
            }
            Err(OakError::RepoLocked) => {}
            Err(error @ OakError::PushCapabilityUnavailable { .. }) => {
                // Deterministic: resending the same head cannot succeed
                // until the server's capabilities change. Park it instead
                // of backing off and retrying forever.
                let message = error.to_string();
                let recheck = blocked_push_recheck();
                let now = unix_now();
                let since = self
                    .state
                    .push_blocked
                    .as_ref()
                    .filter(|blocked| blocked.head == head)
                    .map_or(now, |blocked| blocked.since);
                log(&format!(
                    "push blocked ({message}); not retrying {} before a new commit or the {recheck:?} recheck",
                    short(&head)
                ));
                if since == now {
                    self.notice(
                        "push_blocked",
                        format!("oak watch cannot publish branch '{}' yet: {message}. It will try again after the next commit, and otherwise re-ask the server every {} minute(s); run `oak push --plan --json` for the measured totals.", self.branch, recheck.as_secs().div_ceil(60)),
                    );
                }
                self.push_backoff.reset();
                self.state.push_blocked = Some(super::state::PushBlocked {
                    code: "push_capability_unavailable".to_string(),
                    head,
                    since,
                    next_retry_at: now.saturating_add(recheck.as_secs()),
                });
                self.state.last_push_error = Some(message);
            }
            Err(error) => {
                let message = error.to_string();
                let wait = self.push_backoff.fail();
                self.next_push = Instant::now() + wait;
                log(&format!("push failed ({message}); retrying in {wait:?}"));
                if self.state.last_push_error.as_deref() != Some(message.as_str()) {
                    self.notice(
                        "push_failed",
                        format!("oak watch could not push branch '{}': {message}. It will keep retrying; run `oak push` to see the full error.", self.branch),
                    );
                }
                self.state.push_blocked = None;
                self.state.last_push_error = Some(message);
            }
        }
    }

    /// Send the checkout's uncommitted text files if they changed. Everything
    /// here is best effort: a failure only leaves the branch page behind.
    async fn maybe_share_working(&mut self) {
        self.next_working = Instant::now() + WORKING_INTERVAL;
        if !self.settings.share_working.value {
            self.clear_working().await;
            self.state.working = None;
            self.state.working_files = None;
            self.state.working_skipped = None;
            return;
        }
        // Mid-commit or mid-merge the tree is about to change anyway.
        if self.operation_in_progress() || self.workdir_locked() {
            return;
        }

        let started = Instant::now();
        let (oak_dir, work_tree) = (self.oak_dir.clone(), self.work_tree.clone());
        let scanned =
            tokio::task::spawn_blocking(move || working::scan(&oak_dir, &work_tree)).await;
        let spent = started.elapsed();
        let (head, snapshot) = match scanned {
            Ok(Ok(Some((head, snapshot)))) => (head.to_string(), snapshot),
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                log(&format!("could not scan the working tree: {error}"));
                self.next_working = Instant::now() + self.working_backoff.fail();
                return;
            }
            Err(error) => {
                log(&format!("working tree scan failed: {error}"));
                self.next_working = Instant::now() + self.working_backoff.fail();
                return;
            }
        };
        self.next_working = Instant::now() + WORKING_INTERVAL.max(spent * WORKING_SCAN_SHARE);
        self.state.working_skipped = Some(snapshot.skipped);

        if snapshot.files.is_empty() {
            self.clear_working().await;
            self.state.working = Some("shared".to_string());
            self.state.working_files = Some(0);
            return;
        }
        let fingerprint = snapshot.fingerprint(&head);
        if self.working_sent.as_ref().is_some_and(|sent| {
            sent.fingerprint == fingerprint && sent.at.elapsed() < WORKING_KEEPALIVE
        }) {
            return;
        }

        let sent = working::put_working(
            &self.client,
            &self.remote.url,
            &self.remote.repo_path(),
            &self.branch,
            self.remote.api_key.as_deref(),
            &head,
            &self.working_label,
            &snapshot.files,
        )
        .await;
        match sent {
            Ok(status) if status.is_success() => {
                self.working_backoff.reset();
                self.state.working = Some("shared".to_string());
                self.state.working_files = Some(snapshot.files.len());
                self.working_sent = Some(SentWorking {
                    branch: self.branch.clone(),
                    head,
                    fingerprint,
                    at: Instant::now(),
                });
            }
            // The head isn't on the server yet; `maybe_push` will get it there.
            Ok(status) if status == reqwest::StatusCode::CONFLICT => {
                self.state.working = Some("waiting_for_push".to_string());
                self.next_working = Instant::now() + WORKING_PUSH_WAIT;
            }
            Ok(status) if matches!(status.as_u16(), 401 | 403 | 404) => {
                self.state.working = Some(
                    if status.as_u16() == 404 {
                        "unsupported"
                    } else {
                        "forbidden"
                    }
                    .to_string(),
                );
                self.next_working = Instant::now() + UNSUPPORTED_RETRY;
            }
            Ok(status) if matches!(status.as_u16(), 413 | 422) => {
                log(&format!("server refused the working snapshot: {status}"));
                self.state.working = Some("refused".to_string());
                self.next_working = Instant::now() + WORKING_REFUSED_WAIT;
            }
            Ok(status) => {
                self.state.working = Some("error".to_string());
                let wait = self.working_backoff.fail();
                log(&format!(
                    "sharing work failed ({status}); retrying in {wait:?}"
                ));
                self.next_working = Instant::now() + wait;
            }
            Err(error) => {
                self.state.working = Some("error".to_string());
                let wait = self.working_backoff.fail();
                log(&format!(
                    "sharing work failed ({error}); retrying in {wait:?}"
                ));
                self.next_working = Instant::now() + wait;
            }
        }
    }

    /// Clear what the server holds. It goes under the head that was shared,
    /// because after a commit the new head may not be pushed yet, and the
    /// server refuses a head it doesn't have.
    async fn clear_working(&mut self) {
        let Some(sent) = self.working_sent.take() else {
            return;
        };
        let cleared = clear_snapshot(&self.client, &self.remote, &sent, &self.working_label).await;
        if !cleared.is_ok_and(|status| status.is_success()) {
            log("could not clear the shared work; the server will expire it");
        }
    }

    async fn heartbeat(&mut self) {
        match send_activity(&self.client, &self.remote, &self.branch, "active").await {
            Ok(status) if status.is_success() => {
                self.heartbeat_backoff.reset();
                self.heartbeat_ok_once = true;
                self.state.heartbeat = Some("ok".to_string());
                self.state.last_heartbeat_at = Some(unix_now());
                self.next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
            }
            Ok(status) => {
                // 404: an older server / `oak serve`, or the branch is not on
                // the server yet. 401/403: no write access. Neither improves
                // quickly, so check back rarely.
                self.state.heartbeat = Some(
                    match status.as_u16() {
                        404 => "unsupported",
                        401 | 403 => "forbidden",
                        _ => "error",
                    }
                    .to_string(),
                );
                self.next_heartbeat = Instant::now()
                    + if matches!(status.as_u16(), 401 | 403 | 404) {
                        UNSUPPORTED_RETRY
                    } else {
                        self.heartbeat_backoff.fail()
                    };
            }
            Err(error) => {
                self.state.heartbeat = Some("error".to_string());
                let wait = self.heartbeat_backoff.fail();
                log(&format!("heartbeat failed ({error}); retrying in {wait:?}"));
                self.next_heartbeat = Instant::now() + wait;
            }
        }
    }

    async fn remote_head(&self, branch: &str) -> Result<Option<Hash>> {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            crate::commands::push::fetch_remote_branch_head(
                &self.client,
                &self.remote.url,
                &self.remote.repo_path(),
                branch,
                self.remote.api_key.as_deref(),
            ),
        )
        .await
        .map_err(|_| OakError::Http(format!("timed out reading remote '{branch}'")))?
    }

    /// The branch's remote head and status, best-effort (any failure reads
    /// as unknown; the main check already proved the server reachable).
    async fn remote_branch(&self, branch: &str) -> RemoteBranch {
        let url = format!(
            "{}/api/{}/branches/{}",
            self.remote.url,
            self.remote.repo_path(),
            crate::commands::branch_api_segment(branch)
        );
        let mut request = self.client.get(url).timeout(REQUEST_TIMEOUT);
        if let Some(key) = &self.remote.api_key {
            request = request.bearer_auth(key);
        }
        let Ok(response) = request.send().await else {
            return RemoteBranch::default();
        };
        if !response.status().is_success() {
            return RemoteBranch::default();
        }
        let Ok(body) = response.json::<serde_json::Value>().await else {
            return RemoteBranch::default();
        };
        RemoteBranch {
            head: body
                .get("head")
                .and_then(|h| h.as_str())
                .map(|h| Hash(h.to_string())),
            status: Ok(body
                .get("status")
                .and_then(|s| s.as_str())
                .map(str::to_ascii_lowercase)),
        }
    }
}

struct RemoteBranch {
    head: Option<Hash>,
    status: std::result::Result<Option<String>, ()>,
}

impl Default for RemoteBranch {
    fn default() -> Self {
        Self {
            head: None,
            status: Err(()),
        }
    }
}

fn source_tag(source: config::Source) -> &'static str {
    match source {
        config::Source::Env => "env",
        config::Source::Repo => "repo_config",
        config::Source::Global => "global_config",
        config::Source::Server => "server_preference",
        config::Source::Default => "default",
    }
}

/// `POST /api/{owner}/{name}/branches/{branch}/activity`.
async fn clear_snapshot(
    client: &reqwest::Client,
    remote: &Remote,
    sent: &SentWorking,
    label: &str,
) -> std::result::Result<reqwest::StatusCode, reqwest::Error> {
    working::put_working(
        client,
        &remote.url,
        &remote.repo_path(),
        &sent.branch,
        remote.api_key.as_deref(),
        &sent.head,
        label,
        &[],
    )
    .await
}

async fn send_activity(
    client: &reqwest::Client,
    remote: &Remote,
    branch: &str,
    state: &str,
) -> std::result::Result<reqwest::StatusCode, reqwest::Error> {
    post_activity(
        client,
        &remote.url,
        &remote.repo_path(),
        branch,
        remote.api_key.as_deref(),
        state,
    )
    .await
}

pub(crate) async fn post_activity(
    client: &reqwest::Client,
    remote: &str,
    repo_path: &str,
    branch: &str,
    api_key: Option<&str>,
    state: &str,
) -> std::result::Result<reqwest::StatusCode, reqwest::Error> {
    let mut request = client
        .post(format!(
            "{remote}/api/{repo_path}/branches/{}/activity",
            crate::commands::branch_api_segment(branch)
        ))
        .timeout(Duration::from_secs(5))
        .json(&serde_json::json!({
            "state": state,
            "ttl_secs": HEARTBEAT_TTL_SECS,
            "client": "oak-watch",
        }));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    Ok(request.send().await?.status())
}

/// Paths whose content differs between two commits' snapshots.
fn changed_paths(
    repo: &dyn Repository,
    before: Option<&Hash>,
    after: Option<&Hash>,
) -> Vec<String> {
    let entries = |hash: Option<&Hash>| -> std::collections::BTreeMap<String, String> {
        hash.and_then(|hash| repo.get_commit(hash).ok().flatten())
            .and_then(|commit| repo.get_manifest(&commit.manifest_hash).ok().flatten())
            .map(|manifest| {
                manifest
                    .entries
                    .into_iter()
                    .map(|entry| (entry.path, entry.blob_hash.0))
                    .collect()
            })
            .unwrap_or_default()
    };
    let old = entries(before);
    let new = entries(after);
    let mut changed: Vec<String> = new
        .iter()
        .filter(|(path, blob)| old.get(*path) != Some(*blob))
        .map(|(path, _)| path.clone())
        .chain(old.keys().filter(|path| !new.contains_key(*path)).cloned())
        .collect();
    changed.sort();
    changed
}

/// Registers the handlers when called (not when first polled), so a signal
/// that arrives before the loop's first `select!` is still caught.
#[cfg(unix)]
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let streams = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    );
    async move {
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) = streams else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = hup.recv() => {}
        }
    }
}

#[cfg(not(unix))]
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    async {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn activity_posts_the_contract_body() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::http::ensure_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/acme/demo/branches/a%2Fb/activity"))
            .and(header("authorization", "Bearer key"))
            .and(body_json(serde_json::json!({
                "state": "active", "ttl_secs": 120, "client": "oak-watch"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "branch": "a/b", "active": true, "active_until": null, "actors": []
            })))
            .mount(&server)
            .await;
        let status = post_activity(
            &crate::http::api_client(),
            &server.uri(),
            "acme/demo",
            "a/b",
            Some("key"),
            "active",
        )
        .await
        .unwrap();
        assert!(status.is_success());
        // Unknown route (older server): a plain 404 the caller degrades on.
        let status = post_activity(
            &crate::http::api_client(),
            &server.uri(),
            "acme/demo",
            "other",
            None,
            "idle",
        )
        .await
        .unwrap();
        assert_eq!(status.as_u16(), 404);
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(100));
        assert_eq!(backoff.fail(), Duration::from_secs(100));
        assert_eq!(backoff.fail(), Duration::from_secs(200));
        assert_eq!(backoff.fail(), MAX_BACKOFF);
        assert_eq!(backoff.fail(), MAX_BACKOFF);
        backoff.reset();
        assert_eq!(backoff.fail(), Duration::from_secs(100));
    }
}
