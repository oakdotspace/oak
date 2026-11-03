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
//! - `log` (+ `log.1`) — the daemon's stdout/stderr.

use std::fs;
use std::path::{Path, PathBuf};

use oak_core::Result;
use serde::{Deserialize, Serialize};

/// Notices kept in `state.json`; older ones are dropped.
const MAX_NOTICES: usize = 50;
/// Rotate the log at daemon start once it grows past this.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub struct WatchPaths {
    pub dir: PathBuf,
    pub lock: PathBuf,
    pub pid: PathBuf,
    pub state: PathBuf,
    pub cursor: PathBuf,
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
            log: dir.join("log"),
            dir,
        }
    }

    pub fn load_state(&self) -> Option<WatchState> {
        let text = fs::read_to_string(&self.state).ok()?;
        serde_json::from_str(&text).ok()
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
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
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
