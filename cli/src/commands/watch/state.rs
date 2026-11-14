//! On-disk state of a checkout's watch daemon, under `.oak/watch/`.
//!
//! - `daemon.lock` — advisory lock the live daemon holds for its whole life.
//!   It is the source of truth for "running": the kernel drops it when the
//!   daemon dies, so a stale pidfile can never make a reused pid look like
//!   a daemon (and `oak watch stop` never signals a stranger).
//! - `pid` — the daemon's pid, for `stop` and `status`.
//! - `state.json` — written only by the daemon: what it last did and the
//!   notices it wants the agent to see.
//! - `cursor` — written only by the hook: the last notice id it delivered,
//!   so each notice reaches the agent once. Keeping the two writers on
//!   separate files means neither can clobber the other.
//! - `sessions/<id>` — written only by the hook: the unix time each agent
//!   session last did anything. It tells a session ending from the last one
//!   ending, and tells the daemon that a quiet branch still has an agent.
//! - `log` (+ `log.1`) — the daemon's stdout/stderr.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oak_core::Result;
use serde::{Deserialize, Serialize};

/// Notices kept in `state.json`; older ones are dropped.
const MAX_NOTICES: usize = 50;
/// Rotate the log at daemon start once it grows past this.
const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// Longest hook session id used as a file name.
const MAX_SESSION_ID_LEN: usize = 128;
/// How many times, and how far apart, a starting daemon tries `daemon.lock`.
/// [`WatchPaths::daemon_running`] probes by taking it for an instant, so one
/// refusal does not prove a daemon holds it.
const LOCK_ATTEMPTS: u32 = 20;
const LOCK_ATTEMPT_GAP: Duration = Duration::from_millis(5);
/// The `stop_reason` of a daemon that stopped for lack of activity.
pub const IDLE_STOP_REASON: &str = "idle_timeout";

pub struct WatchPaths {
    pub dir: PathBuf,
    pub lock: PathBuf,
    pub pid: PathBuf,
    pub state: PathBuf,
    pub cursor: PathBuf,
    pub sessions: PathBuf,
    pub log: PathBuf,
}

impl WatchPaths {
    pub fn new(oak_dir: &Path) -> Self {
        let dir = oak_dir.join("watch");
        Self {
            lock: dir.join("daemon.lock"),
            pid: dir.join("pid"),
            state: dir.join("state.json"),
            cursor: dir.join("cursor"),
            sessions: dir.join("sessions"),
            log: dir.join("log"),
            dir,
        }
    }

    pub fn load_state(&self) -> Option<WatchState> {
        let text = fs::read_to_string(&self.state).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Whether daemon `pid` has written its first state. It takes the lock
    /// earlier, so until now the state still describes the previous daemon.
    pub fn start_recorded(&self, pid: u32) -> bool {
        self.load_state()
            .is_some_and(|state| state.pid == Some(pid))
    }

    pub fn save_state(&self, state: &WatchState) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(state)
            .map_err(|e| oak_core::OakError::Io(std::io::Error::other(e)))?;
        crate::atomic_file::write_atomic(&self.state, text)
    }

    pub fn load_cursor(&self) -> u64 {
        fs::read_to_string(&self.cursor)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn save_cursor(&self, id: u64) {
        let _ = fs::create_dir_all(&self.dir);
        let _ = crate::atomic_file::write_atomic(&self.cursor, id.to_string());
    }

    /// The file for a hook-supplied session id, unless the id could escape
    /// `sessions/`.
    fn session_file(&self, id: &str) -> Option<PathBuf> {
        let safe = !id.is_empty()
            && id.len() <= MAX_SESSION_ID_LEN
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        safe.then(|| self.sessions.join(id))
    }

    /// Record that session `id` is active now.
    pub fn touch_session(&self, id: &str) {
        if let Some(path) = self.session_file(id) {
            let _ = fs::create_dir_all(&self.sessions);
            let _ = fs::write(path, unix_now().to_string());
        }
    }

    pub fn remove_session(&self, id: &str) {
        if let Some(path) = self.session_file(id) {
            let _ = fs::remove_file(path);
        }
    }

    /// Every recorded session's last activity, as unix seconds.
    fn session_times(&self) -> Vec<(PathBuf, u64)> {
        let Ok(entries) = fs::read_dir(&self.sessions) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let at = fs::read_to_string(entry.path()).ok()?.trim().parse().ok()?;
                Some((entry.path(), at))
            })
            .collect()
    }

    /// When any session was last active.
    pub fn last_session_activity(&self) -> Option<u64> {
        self.session_times().into_iter().map(|(_, at)| at).max()
    }

    /// Whether a session was active in the last `window_secs`. Older files
    /// are crashed sessions that never reached SessionEnd, so they are dropped.
    pub fn any_session_live(&self, window_secs: u64) -> bool {
        let now = unix_now();
        let mut live = false;
        for (path, at) in self.session_times() {
            if now.saturating_sub(at) < window_secs {
                live = true;
            } else {
                let _ = fs::remove_file(path);
            }
        }
        live
    }

    pub fn load_pid(&self) -> Option<u32> {
        fs::read_to_string(&self.pid).ok()?.trim().parse().ok()
    }

    /// Move an oversized log aside before a new daemon appends to it.
    pub fn rotate_log(&self) {
        if fs::metadata(&self.log).is_ok_and(|md| md.len() > MAX_LOG_BYTES) {
            let _ = fs::rename(&self.log, self.dir.join("log.1"));
        }
    }

    /// Whether a live daemon holds `daemon.lock`. Probes by trying the lock
    /// (and releasing it at once), so it never blocks.
    pub fn daemon_running(&self) -> bool {
        let Ok(file) = fs::OpenOptions::new().read(true).open(&self.lock) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(fs::TryLockError::WouldBlock) => true,
            // A filesystem without advisory locks: fall back to the pidfile.
            Err(fs::TryLockError::Error(_)) => self.load_pid().is_some_and(|pid| {
                !matches!(
                    crate::workdir_lock::process_liveness(pid),
                    crate::workdir_lock::ProcessLiveness::Dead
                )
            }),
        }
    }

    /// The pid of the running daemon, if one is running.
    pub fn running_pid(&self) -> Option<u32> {
        self.daemon_running().then(|| self.load_pid()).flatten()
    }
}

/// The lock a running daemon holds; dropping it (or dying) releases it.
pub struct DaemonLock {
    _file: fs::File,
    pid_path: PathBuf,
}

impl DaemonLock {
    /// Take the singleton lock for this checkout. `Ok(None)` when another
    /// daemon already holds it.
    pub fn acquire(paths: &WatchPaths) -> Result<Option<Self>> {
        fs::create_dir_all(&paths.dir)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        let mut attempts = 1;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if attempts < LOCK_ATTEMPTS => {
                    attempts += 1;
                    std::thread::sleep(LOCK_ATTEMPT_GAP);
                }
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        fs::write(&paths.pid, std::process::id().to_string())?;
        Ok(Some(Self {
            _file: file,
            pid_path: paths.pid.clone(),
        }))
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.pid_path)
            .is_ok_and(|pid| pid.trim() == std::process::id().to_string())
        {
            let _ = fs::remove_file(&self.pid_path);
        }
    }
}

/// A message for the agent, delivered once by the hook.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notice {
    pub id: u64,
    pub at: u64,
    /// `main_merged`, `conflict`, `conflict_resolved`, `pull_aborted`,
    /// `pull_failed`, `push_failed`, `checkpoint_failed`, `stopped`.
    pub kind: String,
    pub message: String,
}

/// A parent-sync conflict the daemon produced and is waiting on.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingConflict {
    /// Remote main head the conflicted pull merged.
    pub main_head: Option<String>,
    pub paths: Vec<String>,
    pub at: u64,
}

/// Auto-push is parked on a deterministic server-capability refusal: the
/// same push fails the same way until the branch head or the server's
/// capabilities change, so the daemon does not keep resending it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushBlocked {
    /// The push error's stable JSON code (`push_capability_unavailable`).
    pub code: String,
    /// The local branch head the refusal applies to.
    pub head: String,
    pub since: u64,
    /// When the daemon next re-asks the server for this same head.
    pub next_retry_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WatchState {
    pub schema_version: u32,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub branch: Option<String>,
    /// `sse` while the event stream is connected, else `poll`.
    #[serde(default)]
    pub transport: Option<String>,
    /// `ok`, `unsupported` (404: older server or `oak serve`), `forbidden`,
    /// or `error`.
    #[serde(default)]
    pub heartbeat: Option<String>,
    #[serde(default)]
    pub last_heartbeat_at: Option<u64>,
    /// Remote main head as of the last check.
    #[serde(default)]
    pub last_main_head: Option<String>,
    #[serde(default)]
    pub last_check_at: Option<u64>,
    #[serde(default)]
    pub last_pull_at: Option<u64>,
    /// The pull receipt's `convergence` (`merged`, `conflict`, ...).
    #[serde(default)]
    pub last_pull_result: Option<String>,
    #[serde(default)]
    pub last_pushed_head: Option<String>,
    #[serde(default)]
    pub last_push_at: Option<u64>,
    #[serde(default)]
    pub last_push_error: Option<String>,
    #[serde(default)]
    pub push_blocked: Option<PushBlocked>,
    /// How sharing uncommitted work went: `shared`, `unsupported` (404: the
    /// server lacks it or its flag is off), `forbidden`, `refused`,
    /// `waiting_for_push`, or `error`. `None` while the setting is off.
    #[serde(default)]
    pub working: Option<String>,
    #[serde(default)]
    pub working_files: Option<usize>,
    #[serde(default)]
    pub working_skipped: Option<usize>,
    #[serde(default)]
    pub conflict: Option<PendingConflict>,
    /// A main head whose merge the agent aborted: auto-pull waits for main
    /// to move past it instead of re-creating the same conflict.
    #[serde(default)]
    pub skip_main_head: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub stopped_at: Option<u64>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub notices: Vec<Notice>,
    #[serde(default)]
    pub next_notice_id: u64,
}

impl WatchState {
    pub fn push_notice(&mut self, kind: &str, message: String) {
        // Ids continue across daemon restarts so the hook cursor stays valid.
        self.next_notice_id = self.next_notice_id.max(1);
        let id = self.next_notice_id;
        self.next_notice_id += 1;
        self.notices.push(Notice {
            id,
            at: unix_now(),
            kind: kind.to_string(),
            message,
        });
        if self.notices.len() > MAX_NOTICES {
            let excess = self.notices.len() - MAX_NOTICES;
            self.notices.drain(..excess);
        }
    }

    pub fn notices_after(&self, cursor: u64) -> impl Iterator<Item = &Notice> {
        self.notices.iter().filter(move |notice| notice.id > cursor)
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_ids_are_monotonic_and_bounded() {
        let mut state = WatchState::default();
        for n in 0..(MAX_NOTICES + 5) {
            state.push_notice("main_merged", format!("n{n}"));
        }
        assert_eq!(state.notices.len(), MAX_NOTICES);
        assert_eq!(state.notices.first().unwrap().id, 6);
        assert_eq!(state.notices_after(MAX_NOTICES as u64).count(), 5);
        assert_eq!(state.next_notice_id, MAX_NOTICES as u64 + 6);
    }

    #[test]
    fn sessions_track_activity_and_refuse_unsafe_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WatchPaths::new(tmp.path());
        assert_eq!(paths.last_session_activity(), None);
        assert!(!paths.any_session_live(60));

        paths.touch_session("../../escape");
        paths.touch_session("");
        assert!(!tmp.path().join("escape").exists());
        assert_eq!(paths.last_session_activity(), None);

        paths.touch_session("a-1");
        assert!(paths.any_session_live(60));
        assert!(paths.last_session_activity().is_some());

        fs::write(paths.sessions.join("crashed"), "100").unwrap();
        assert!(paths.any_session_live(60));
        assert!(!paths.sessions.join("crashed").exists(), "stale is pruned");

        paths.remove_session("a-1");
        assert!(!paths.any_session_live(60));
    }

    #[test]
    fn a_running_probe_does_not_cost_a_new_daemon_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WatchPaths::new(tmp.path());
        DaemonLock::acquire(&paths).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    paths.daemon_running();
                }
            });
            let refused = (0..2000)
                .filter(|_| DaemonLock::acquire(&paths).unwrap().is_none())
                .count();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(refused, 0, "a probe is not a daemon");
        });
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = WatchPaths::new(tmp.path());
        assert!(!paths.daemon_running());
        let lock = DaemonLock::acquire(&paths).unwrap().expect("first lock");
        assert!(paths.daemon_running());
        assert_eq!(paths.running_pid(), Some(std::process::id()));
        // flock is per open file description, so a second acquire from this
        // process still contends.
        assert!(DaemonLock::acquire(&paths).unwrap().is_none());
        drop(lock);
        assert!(!paths.daemon_running());
        assert!(!paths.pid.exists());
    }
}
