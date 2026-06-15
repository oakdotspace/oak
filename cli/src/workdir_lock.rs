use std::fs;
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
        let lock_path = oak_dir.join("wdlock");

        // Check if an existing lock is held by a live process
        if lock_path.exists() {
            if let Ok(contents) = fs::read_to_string(&lock_path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    if is_process_alive(pid) {
                        return Err(OakError::RepoLocked);
                    }
                }
                // Stale lock (dead process) — remove it
                let _ = fs::remove_file(&lock_path);
            }
        }

        // Write our PID atomically (write to temp, rename)
        let pid = std::process::id();
        let tmp_path = oak_dir.join(format!("wdlock.{pid}"));
        fs::write(&tmp_path, pid.to_string())?;

        // Rename is atomic on most filesystems
        if let Err(e) = fs::rename(&tmp_path, &lock_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(OakError::Io(e));
        }

        // Double-check we actually own the lock (handles race with another process)
        if let Ok(contents) = fs::read_to_string(&lock_path) {
            if contents.trim() != pid.to_string() {
                return Err(OakError::RepoLocked);
            }
        }

        Ok(WorkdirLock { lock_path })
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
