use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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
                    if reap_if_owner_dead(&lock_path)? {
                        continue;
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
    ///
    /// Retries use jittered exponential backoff (see [`LockBackoff`]) so many
    /// waiters neither poll in lockstep nor burn CPU the owner needs. The
    /// wait never exceeds `timeout`; on expiry the typed
    /// [`OakError::RepoLocked`] is returned.
    pub fn acquire_wait(oak_dir: &Path, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut backoff = LockBackoff::new();
        loop {
            match Self::acquire(oak_dir) {
                Ok(lock) => return Ok(lock),
                Err(OakError::RepoLocked) => {
                    if !backoff.sleep_before(deadline) {
                        let lock_path = oak_dir.join("wdlock");
                        eprintln!(
                            "oak: gave up after {:.1}s waiting for lock {} ({}); if no other oak process is using it, remove that file",
                            timeout.as_secs_f64(),
                            lock_path.display(),
                            describe_lock_owner(&lock_path)
                        );
                        return Err(OakError::RepoLocked);
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// Jittered exponential backoff for polling a file lock until a deadline.
pub(crate) struct LockBackoff {
    next: Duration,
}

impl LockBackoff {
    const INITIAL: Duration = Duration::from_millis(2);
    const MAX: Duration = Duration::from_millis(25);

    pub(crate) fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    /// Sleep for the next jittered interval, clipped to `deadline`. Returns
    /// false (without sleeping) once the deadline has passed, so the caller
    /// makes exactly one attempt at or after the deadline before giving up.
    pub(crate) fn sleep_before(&mut self, deadline: Instant) -> bool {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        // "Equal jitter": uniformly in [next/2, next].
        let half = self.next / 2;
        let jitter_nanos = u64::try_from(half.as_nanos()).unwrap_or(u64::MAX).max(1);
        let pause = half + Duration::from_nanos(random_u64() % jitter_nanos);
        thread::sleep(pause.min(deadline - now));
        self.next = (self.next * 2).min(Self::MAX);
        true
    }
}

fn random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    RandomState::new().hash_one(std::process::id())
}

/// Remove a lock file only when its recorded owner is provably gone.
///
/// Returns `Ok(true)` when the caller should retry creating the lock (the
/// stale file was removed, or had already vanished) and `Ok(false)` when it
/// must treat the lock as held: a live owner, an owner whose liveness cannot
/// be determined (never treated as dead), or another reaper working on it
/// right now.
///
/// The lock itself stays a create-exclusive file holding the owner's pid, so
/// released clients that only know that protocol still exclude new ones.
/// What makes reaping safe is a companion reaper mutex: an OS advisory lock
/// (`File::try_lock`: flock on Unix, LockFileEx on Windows) on a stable
/// sibling file `<lock>.reap` that is never deleted. The kernel drops it if
/// a reaper dies, so it can never go stale itself. Under that mutex the
/// owner is judged again and the file is removed only if it is still the
/// same file (same contents and, on Unix, the same device/inode) that was
/// judged dead. Among clients with this code, check-then-remove is therefore
/// atomic: a lock another contender has just taken over is never deleted.
///
/// Known limits: a released client that predates the reaper mutex still
/// reaps without it, so a mixed old/new population retains the old race
/// between the two; and liveness is pid-based, so an owner in another pid
/// namespace (a container sharing the directory) or on another host (NFS)
/// reads as dead. If the filesystem refuses advisory locks, nothing is
/// reaped and the lock is reported as held.
pub(crate) fn reap_if_owner_dead(lock_path: &Path) -> Result<bool> {
    // Cheap unlocked pre-check: the common case is a live owner.
    if stale_lock_snapshot(lock_path)?.is_none() {
        return Ok(false);
    }
    let Some(_reaper) = ReaperMutex::try_acquire(lock_path) else {
        return Ok(false);
    };
    match stale_lock_snapshot(lock_path)? {
        Some(snapshot) => remove_if_unchanged(lock_path, &snapshot),
        None => Ok(false),
    }
}

/// Short-lived OS advisory lock serializing reapers of one lock file.
struct ReaperMutex {
    _file: fs::File,
}

impl ReaperMutex {
    const WAIT: Duration = Duration::from_millis(200);

    fn try_acquire(lock_path: &Path) -> Option<Self> {
        let mut name = lock_path.file_name()?.to_os_string();
        name.push(".reap");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let path = lock_path.with_file_name(name);
        // An advisory lock needs no write access, so a companion file this
        // user cannot write (e.g. left by a one-off `sudo oak`) is opened
        // read-only rather than disabling reaping.
        let file = options
            .open(&path)
            .or_else(|_| fs::File::open(&path))
            .ok()?;
        let deadline = Instant::now() + Self::WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return None,
            }
        }
    }
}

/// Describe a lock file's recorded owner for error messages.
pub(crate) fn describe_lock_owner(lock_path: &Path) -> String {
    match fs::read_to_string(lock_path) {
        Ok(contents) => match contents.trim().parse::<u32>() {
            Ok(pid) => match process_liveness(pid) {
                ProcessLiveness::Alive => format!("held by pid {pid}"),
                ProcessLiveness::Unknown => {
                    format!("held by pid {pid}, whose liveness cannot be determined")
                }
                ProcessLiveness::Dead => {
                    format!("recorded owner pid {pid} has exited; the lock is being reaped")
                }
            },
            Err(_) => "owner pid not yet recorded".to_string(),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => "now released".to_string(),
        Err(e) => format!("owner unreadable: {e}"),
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

#[derive(Debug)]
struct LockSnapshot {
    contents: String,
    len: u64,
    modified: Option<SystemTime>,
    identity: Option<FileIdentity>,
}

type FileIdentity = (u64, u64);

#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &fs::Metadata) -> Option<FileIdentity> {
    None
}

/// `Some` when the lock at `lock_path` may be reaped: it vanished (empty
/// snapshot), its owner is provably dead, or it is malformed and old enough
/// to be a crashed partial acquisition.
fn stale_lock_snapshot(lock_path: &Path) -> Result<Option<LockSnapshot>> {
    let vanished = || LockSnapshot {
        contents: String::new(),
        len: 0,
        modified: None,
        identity: None,
    };
    let meta = match fs::metadata(lock_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Some(vanished())),
        Err(e) => return Err(OakError::Io(e)),
    };
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Some(vanished())),
        Err(e) => return Err(OakError::Io(e)),
    };
    let snapshot = LockSnapshot {
        len: meta.len(),
        modified: meta.modified().ok(),
        identity: file_identity(&meta),
        contents,
    };
    let Ok(pid) = snapshot.contents.trim().parse::<u32>() else {
        // Another process may have atomically created the lockfile but not
        // written its PID yet. Treat malformed contents as locked rather than
        // deleting a live contender's lock; only reclaim it after it is old
        // enough to be a crashed partial acquisition.
        return Ok(lock_age(lock_path)
            .is_some_and(|age| age > Duration::from_secs(30))
            .then_some(snapshot));
    };
    Ok(matches!(process_liveness(pid), ProcessLiveness::Dead).then_some(snapshot))
}

/// Remove `lock_path` if it is still exactly the file `snapshot` judged
/// stale. Only called with the [`ReaperMutex`] held, which is what makes
/// the compare and the removal atomic with respect to other reapers.
fn remove_if_unchanged(lock_path: &Path, snapshot: &LockSnapshot) -> Result<bool> {
    if snapshot.modified.is_none() && snapshot.contents.is_empty() {
        return Ok(true);
    }
    let meta = match fs::metadata(lock_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(OakError::Io(e)),
    };
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(OakError::Io(e)),
    };
    if contents != snapshot.contents
        || meta.len() != snapshot.len
        || meta.modified().ok() != snapshot.modified
        || file_identity(&meta) != snapshot.identity
    {
        return Ok(false);
    }
    match fs::remove_file(lock_path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
        Err(e) => Err(OakError::Io(e)),
    }
}

fn lock_age(lock_path: &Path) -> Option<Duration> {
    fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessLiveness {
    Alive,
    Dead,
    Unknown,
}

pub(crate) fn process_liveness(pid: u32) -> ProcessLiveness {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return ProcessLiveness::Alive;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => ProcessLiveness::Dead,
            Some(libc::EPERM) => ProcessLiveness::Unknown,
            _ => ProcessLiveness::Unknown,
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FALSE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) {
                Ok(handle) => handle,
                Err(error) => {
                    // ERROR_INVALID_PARAMETER is Windows' documented result
                    // for a PID that no longer names a process. Access denied
                    // and every unclassified failure are conservative unknowns.
                    return if error.code() == ERROR_INVALID_PARAMETER.to_hresult() {
                        ProcessLiveness::Dead
                    } else {
                        ProcessLiveness::Unknown
                    };
                }
            };
            let mut code = 0u32;
            let result = GetExitCodeProcess(handle, &mut code);
            let _ = CloseHandle(handle);
            match result {
                Ok(()) if code == STILL_ACTIVE => ProcessLiveness::Alive,
                Ok(()) => ProcessLiveness::Dead,
                Err(_) => ProcessLiveness::Unknown,
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        ProcessLiveness::Unknown
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
    fn acquire_wait_on_live_owner_is_bounded_and_typed() {
        let temp = tempfile::tempdir().unwrap();
        let oak_dir = temp.path().join(".oak");
        let held = WorkdirLock::acquire(&oak_dir).unwrap();

        let started = Instant::now();
        let result = WorkdirLock::acquire_wait(&oak_dir, Duration::from_millis(200));
        let waited = started.elapsed();

        assert!(matches!(result, Err(OakError::RepoLocked)));
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(waited < Duration::from_secs(3), "{waited:?}");
        drop(held);
        WorkdirLock::acquire_wait(&oak_dir, Duration::from_millis(200)).unwrap();
    }

    fn exited_pid() -> u32 {
        let mut child = Command::new(env::current_exe().unwrap())
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        pid
    }

    /// Adopted from independent QA (QA-L8 F1, qa_l8_toctou): contenders that
    /// race to reap the same dead-owner lock must never both end up holding
    /// it. `OAK_TEST_REAP_ROUNDS` raises the round count for stress runs.
    #[test]
    fn concurrent_reapers_of_a_dead_owner_lock_never_double_acquire() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let rounds: usize = env::var("OAK_TEST_REAP_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let threads = 8;
        let temp = tempfile::tempdir().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir_all(&oak_dir).unwrap();
        let lock_path = oak_dir.join("wdlock");
        let dead = exited_pid();
        let mut double_rounds = 0;
        let mut acquisitions = 0;
        for _ in 0..rounds {
            let _ = fs::remove_file(&lock_path);
            fs::write(&lock_path, dead.to_string()).unwrap();
            let barrier = Arc::new(Barrier::new(threads));
            let holders = Arc::new(AtomicUsize::new(0));
            let max_holders = Arc::new(AtomicUsize::new(0));
            let acquired = Arc::new(AtomicUsize::new(0));
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let holders = Arc::clone(&holders);
                    let max_holders = Arc::clone(&max_holders);
                    let acquired = Arc::clone(&acquired);
                    let oak_dir = oak_dir.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        if let Ok(lock) = WorkdirLock::acquire(&oak_dir) {
                            let now = holders.fetch_add(1, Ordering::SeqCst) + 1;
                            max_holders.fetch_max(now, Ordering::SeqCst);
                            acquired.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(3));
                            holders.fetch_sub(1, Ordering::SeqCst);
                            // Every thread shares this pid, so Drop could
                            // remove another thread's lock; the next round
                            // resets the file instead.
                            std::mem::forget(lock);
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
            acquisitions += acquired.load(Ordering::SeqCst);
            if max_holders.load(Ordering::SeqCst) > 1 {
                double_rounds += 1;
            }
        }
        eprintln!(
            "reap stress: rounds={rounds} threads={threads} acquisitions={acquisitions} double_acquire_rounds={double_rounds}"
        );
        assert!(acquisitions >= rounds, "every round must reap and acquire");
        assert_eq!(
            double_rounds, 0,
            "stale-lock reaping let two holders acquire concurrently"
        );
    }

    #[test]
    fn lock_owner_description_names_the_recorded_pid() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("wdlock");
        assert_eq!(describe_lock_owner(&lock_path), "now released");
        let me = std::process::id();
        fs::write(&lock_path, format!("{me}\n")).unwrap();
        assert_eq!(describe_lock_owner(&lock_path), format!("held by pid {me}"));
        let dead = exited_pid();
        fs::write(&lock_path, dead.to_string()).unwrap();
        assert_eq!(
            describe_lock_owner(&lock_path),
            format!("recorded owner pid {dead} has exited; the lock is being reaped")
        );
        fs::write(&lock_path, "").unwrap();
        assert_eq!(
            describe_lock_owner(&lock_path),
            "owner pid not yet recorded"
        );
    }

    #[test]
    fn live_owner_is_never_reaped_and_reaper_mutex_file_is_stable() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("wdlock");
        fs::write(&lock_path, std::process::id().to_string()).unwrap();
        assert!(!reap_if_owner_dead(&lock_path).unwrap());
        assert!(lock_path.exists());

        fs::write(&lock_path, exited_pid().to_string()).unwrap();
        assert!(reap_if_owner_dead(&lock_path).unwrap());
        assert!(!lock_path.exists());
        // The advisory-lock companion is never deleted, so it cannot race.
        let reap_path = temp.path().join("wdlock.reap");
        assert!(reap_path.exists());

        // A companion this user cannot write still serves as the mutex.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&reap_path, fs::Permissions::from_mode(0o400)).unwrap();
            fs::write(&lock_path, exited_pid().to_string()).unwrap();
            assert!(reap_if_owner_dead(&lock_path).unwrap());
            assert!(!lock_path.exists());
        }
    }

    #[test]
    fn backoff_never_sleeps_past_the_deadline() {
        let mut backoff = LockBackoff::new();
        let deadline = Instant::now() + Duration::from_millis(60);
        let mut sleeps = 0;
        while backoff.sleep_before(deadline) {
            sleeps += 1;
        }
        let overshoot = Instant::now().saturating_duration_since(deadline);
        assert!(sleeps >= 3, "expected several polls, got {sleeps}");
        assert!(overshoot < Duration::from_millis(50), "{overshoot:?}");
        assert!(!backoff.sleep_before(Instant::now()));
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

    #[test]
    fn exited_process_lock_is_recoverable_without_treating_unknown_as_dead() {
        let temp = tempfile::tempdir().unwrap();
        let oak_dir = temp.path().join(".oak");
        fs::create_dir(&oak_dir).unwrap();
        let mut child = Command::new(env::current_exe().unwrap())
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        fs::write(oak_dir.join("wdlock"), pid.to_string()).unwrap();

        let lock = WorkdirLock::acquire(&oak_dir).unwrap();
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
