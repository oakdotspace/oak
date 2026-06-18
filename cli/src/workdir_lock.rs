use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use oak_core::{OakError, Result};

/// A process-level lock on the working directory to prevent concurrent
/// CLI write operations (commit, reset, restore, merge, pull) from
/// corrupting repository state.
///
/// The lock is released automatically when the guard is dropped.
pub struct WorkdirLock {
    lock_path: PathBuf,
}

impl WorkdirLock {
    /// Acquire an exclusive lock on the working directory.
    /// Returns an error if another process holds the lock.
    pub fn acquire(oak_dir: &Path) -> Result<Self> {
        fs::create_dir_all(oak_dir)?;
        let lock_path = oak_dir.join("wdlock");
        let pid = std::process::id();

        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    if let Err(e) = write!(file, "{pid}") {
                        let _ = fs::remove_file(&lock_path);
                        return Err(OakError::Io(e));
                    }
                    if let Err(e) = file.sync_all() {
                        let _ = fs::remove_file(&lock_path);
                        return Err(OakError::Io(e));
                    }
                    return Ok(WorkdirLock { lock_path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if lock_is_stale(&lock_path)? {
                        match fs::remove_file(&lock_path) {
                            Ok(()) => continue,
                            Err(e) if e.kind() == ErrorKind::NotFound => continue,
                            Err(e) => return Err(OakError::Io(e)),
                        }
                    }
                    return Err(OakError::RepoLocked);
                }
                Err(e) => return Err(OakError::Io(e)),
            }
        }
    }

    /// Acquire an exclusive lock, waiting briefly if another live process owns
    /// it. This keeps short concurrent write operations from forcing callers
    /// into expensive process-level retry loops.
    pub fn acquire_wait(oak_dir: &Path, timeout: Duration) -> Result<Self> {
        let start = Instant::now();
        loop {
            match Self::acquire(oak_dir) {
                Ok(lock) => return Ok(lock),
                Err(OakError::RepoLocked) if start.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for WorkdirLock {
    fn drop(&mut self) {
        // Only remove if we still own it (check PID)
        if let Ok(contents) = fs::read_to_string(&self.lock_path) {
            if contents.trim() == std::process::id().to_string() {
                let _ = fs::remove_file(&self.lock_path);
            }
        }
    }
}

fn lock_is_stale(lock_path: &Path) -> Result<bool> {
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(OakError::Io(e)),
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        // Another process may have atomically created the lockfile but not
        // written its PID yet. Treat malformed contents as locked rather than
        // deleting a live contender's lock; only reclaim it after it is old
        // enough to be a crashed partial acquisition.
        return Ok(lock_age(lock_path).is_some_and(|age| age > Duration::from_secs(30)));
    };
    Ok(!is_process_alive(pid))
}

fn lock_age(lock_path: &Path) -> Option<Duration> {
    fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
}

fn is_process_alive(pid: u32) -> bool {
    // Use /proc on Linux, sysctl-based check on macOS
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(target_os = "macos")]
    {
        // `kill -0` via Command — returns success if process exists
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        // Conservative: assume stale on unknown platforms
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process::Command;

    const CHILD_ENV: &str = "OAK_WDLOCK_CONTENTION_CHILD";
    const LOCK_DIR_ENV: &str = "OAK_WDLOCK_CONTENTION_DIR";
    const READY_DIR_ENV: &str = "OAK_WDLOCK_CONTENTION_READY_DIR";
    const START_ENV: &str = "OAK_WDLOCK_CONTENTION_START";
    const STOP_ENV: &str = "OAK_WDLOCK_CONTENTION_STOP";
    const WINNERS_ENV: &str = "OAK_WDLOCK_CONTENTION_WINNERS";

    #[test]
    fn acquire_excludes_concurrent_processes() {
        if env::var_os(CHILD_ENV).is_some() {
            run_contention_child();
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let ready_dir = temp.path().join("ready");
        fs::create_dir(&ready_dir).unwrap();
        let start_path = temp.path().join("start");
        let stop_path = temp.path().join("stop");
        let winners_path = temp.path().join("winners");
        let child_count = 8;

        let mut children = Vec::new();
        for _ in 0..child_count {
            children.push(
                Command::new(env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("workdir_lock::tests::acquire_excludes_concurrent_processes")
                    .arg("--nocapture")
                    .env(CHILD_ENV, "1")
                    .env(LOCK_DIR_ENV, &oak_dir)
                    .env(READY_DIR_ENV, &ready_dir)
                    .env(START_ENV, &start_path)
                    .env(STOP_ENV, &stop_path)
                    .env(WINNERS_ENV, &winners_path)
                    .spawn()
                    .unwrap(),
            );
        }

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready_count = fs::read_dir(&ready_dir).unwrap().count();
            if ready_count == child_count {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "timed out waiting for children to reach the lock barrier"
            );
            thread::sleep(Duration::from_millis(2));
        }

        fs::write(&start_path, b"go").unwrap();

        let count_deadline = Instant::now() + Duration::from_secs(5);
        while !winners_path.exists() {
            assert!(
                Instant::now() < count_deadline,
                "timed out waiting for a lock winner"
            );
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(100));

        let winners = fs::read_to_string(&winners_path).unwrap_or_default();
        let winner_count = winners.lines().count();
        assert_eq!(
            winner_count, 1,
            "exactly one process should acquire the lock, got {winner_count}: {winners:?}"
        );

        fs::write(&stop_path, b"stop").unwrap();
        for mut child in children {
            let status = child.wait().unwrap();
            assert!(status.success(), "contention child failed: {status}");
        }
    }

    #[test]
    fn acquire_creates_missing_lock_directory() {
        let temp = tempfile::tempdir().unwrap();
        let oak_dir = temp.path().join(".git/oak");
        assert!(!oak_dir.exists());

        let lock = WorkdirLock::acquire(&oak_dir).unwrap();

        assert!(oak_dir.is_dir());
        assert!(oak_dir.join("wdlock").is_file());
        drop(lock);
        assert!(!oak_dir.join("wdlock").exists());
    }

    fn run_contention_child() {
        let oak_dir = PathBuf::from(env::var_os(LOCK_DIR_ENV).unwrap());
        let ready_dir = PathBuf::from(env::var_os(READY_DIR_ENV).unwrap());
        let start_path = PathBuf::from(env::var_os(START_ENV).unwrap());
        let stop_path = PathBuf::from(env::var_os(STOP_ENV).unwrap());
        let winners_path = PathBuf::from(env::var_os(WINNERS_ENV).unwrap());
        fs::write(
            ready_dir.join(std::process::id().to_string()),
            std::process::id().to_string(),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !start_path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for start file"
            );
            thread::sleep(Duration::from_millis(2));
        }

        match WorkdirLock::acquire(&oak_dir) {
            Ok(_lock) => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&winners_path)
                    .unwrap();
                writeln!(file, "{}", std::process::id()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !stop_path.exists() {
                    assert!(Instant::now() < deadline, "timed out waiting for stop file");
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Err(OakError::RepoLocked) => {}
            Err(e) => panic!("unexpected lock error: {e}"),
        }
    }
}
