//! Spawning a mount as a detached background daemon.
//!
//! The mount's virtual filesystem is served by an in-process daemon (the FUSE
//! event loop on Linux, the FSKit IPC server on macOS, the ProjFS callbacks on
//! Windows). That server lives for the life of the `oak mount __serve` process,
//! so to hand the terminal back we spawn it *detached* — in its own session, so
//! it survives the launching shell — then poll until the kernel reports the
//! path as a live mountpoint before returning.
//!
//! Both the user-facing `oak mount` driver and the Claude Code `WorktreeCreate`
//! hook use [`spawn_detached`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oak_core::{OakError, Result};

use super::state::{
    config_path, load_daemon_pid, lookup_id_for, pid_alive, state_dir_for, unregister_mount,
};
use crate::output;

/// How long to wait for a freshly spawned mount to become a live mountpoint
/// before giving up. Fetching the manifest + blob sizes on first mount can
/// take a few seconds on a large repo, so this is generous.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Readiness polling: a typical mount comes live in ~1-2s, and the poll
/// grid's quantization lands directly on `oak mount`'s wall time (the mount
/// is ready mid-sleep). `is_ready` is a registry read plus two stats, so
/// poll finely while a normal startup could complete, then back off for the
/// large-repo tail.
const POLL_INTERVAL_FAST: Duration = Duration::from_millis(10);
const POLL_INTERVAL_SLOW: Duration = Duration::from_millis(150);
const POLL_FAST_WINDOW: Duration = Duration::from_secs(5);

/// What `spawn_detached` should do for a destination, based on what's
/// actually there — not just whether an `index.json` entry exists. A registry
/// entry alone proves nothing: after a reboot or daemon crash the entry
/// survives while nothing serves files, and treating it as "already mounted"
/// silently hands the caller a dead plain directory.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnPlan {
    /// Registered and the kernel reports a live mountpoint — nothing to do.
    AlreadyLive,
    /// Registered, not mounted, but the mount's state dir is intact: respawn
    /// the daemon over the existing state (virtual branch, overlay, cache).
    Respawn,
    /// No usable registration (none at all, or a stale entry whose state dir
    /// is gone — which `plan_spawn` removes): proceed with a fresh mount.
    Fresh,
}

/// Classify `dest` for mounting. Cleans up a stale registry entry whose
/// state dir has been deleted, since there is nothing left to resume from.
pub fn plan_spawn(dest: &Path) -> Result<SpawnPlan> {
    let Some(id) = lookup_id_for(dest)? else {
        return Ok(SpawnPlan::Fresh);
    };
    if is_mountpoint(dest) {
        return Ok(SpawnPlan::AlreadyLive);
    }
    let state_dir = state_dir_for(&id)?;
    if config_path(&state_dir).exists() {
        return Ok(SpawnPlan::Respawn);
    }
    // Registered but nothing to serve and nothing to resume — drop the dead
    // entry so the fresh mount doesn't trip over it.
    unregister_mount(dest)?;
    Ok(SpawnPlan::Fresh)
}

/// Spawn `oak mount __serve … <spec> <dest>` as a detached background daemon
/// and block until it serves a live mountpoint at `dest`.
///
/// Idempotent: if a *live* mount is already at `dest`, this is a no-op. A
/// stale registration (daemon died, machine rebooted) is respawned from its
/// state dir, or cleaned up and remounted fresh when the state dir is gone.
/// On a fresh-mount failure (the server exits early or never comes up) the
/// half-created mount is cleaned up and the server's log is surfaced in the
/// error; a failed respawn leaves the state dir intact, since it may hold
/// committed-but-unpushed work.
pub fn spawn_detached(remote: &str, spec: &str, dest: &Path) -> Result<()> {
    match plan_spawn(dest)? {
        SpawnPlan::AlreadyLive => return Ok(()),
        SpawnPlan::Respawn => return respawn_detached(dest),
        SpawnPlan::Fresh => {}
    }

    let mut child = launch(dest, |cmd| {
        cmd.arg("mount").arg("__serve").arg("--remote").arg(remote);
        cmd.arg(spec).arg(dest);
    })?;
    wait_ready(&mut child, dest, &format!("mounting {spec}"), true)
}

/// Respawn the daemon for an existing-but-dead mount registration, reusing
/// its state dir (`oak mount __resume <dest>`). Never deletes state on
/// failure — unlike a fresh mount, the state dir may be the only copy of
/// committed-but-unpushed work.
pub fn respawn_detached(dest: &Path) -> Result<()> {
    // A recorded daemon pid that is still alive but not serving a mountpoint
    // is a hung or half-started daemon; clear it out so the respawn doesn't
    // fight it over the state dir.
    if let Some(id) = lookup_id_for(dest)? {
        let state_dir = state_dir_for(&id)?;
        if let Some(pid) = load_daemon_pid(&state_dir) {
            if pid != std::process::id() && pid_alive(pid) {
                terminate_pid(pid);
            }
        }
    }

    // Stderr, not stdout: the WorktreeCreate hook path runs through here and
    // its stdout must carry nothing but the worktree path.
    output::warning(&format!(
        "mount at {} was registered but not live — respawning its daemon",
        dest.display()
    ));
    let mut child = launch(dest, |cmd| {
        cmd.arg("mount").arg("__resume").arg(dest);
    })?;
    wait_ready(&mut child, dest, "respawning the mount", false)
}

/// Start a detached `oak mount …` daemon with its output teed to the
/// per-destination log file, so a failed mount leaves a diagnosable trail.
fn launch(
    dest: &Path,
    args: impl FnOnce(&mut std::process::Command),
) -> Result<std::process::Child> {
    let log_path = log_path_for(dest);
    let log = fs::File::create(&log_path)?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe().map_err(OakError::Io)?;
    let mut cmd = std::process::Command::new(exe);
    args(&mut cmd);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    detach(&mut cmd);
    cmd.spawn().map_err(OakError::Io)
}

/// Block until `dest` is a live mountpoint, the child exits, or the timeout
/// elapses. `cleanup` controls whether a failure tears the half-created mount
/// down (fresh mounts) or leaves its state in place (respawns).
fn wait_ready(
    child: &mut std::process::Child,
    dest: &Path,
    what: &str,
    cleanup: bool,
) -> Result<()> {
    let log_path = log_path_for(dest);
    let started = Instant::now();
    let deadline = started + READY_TIMEOUT;
    loop {
        if is_ready(dest)? {
            return Ok(());
        }
        // If the server died before becoming ready, surface its log rather
        // than spinning until the timeout.
        if let Some(status) = child.try_wait().map_err(OakError::Io)? {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            if cleanup {
                cleanup_failed(dest);
            }
            return Err(OakError::Server(format!(
                "{what} at {} exited ({status}) before it came up:\n{}",
                dest.display(),
                log.trim()
            )));
        }
        if Instant::now() >= deadline {
            // Stop the still-spawning mount; for a fresh mount also leave
            // nothing half-created.
            let _ = child.kill();
            if cleanup {
                cleanup_failed(dest);
            }
            return Err(OakError::Server(format!(
                "timed out after {}s waiting for the mount at {} to come up",
                READY_TIMEOUT.as_secs(),
                dest.display()
            )));
        }
        std::thread::sleep(if started.elapsed() < POLL_FAST_WINDOW {
            POLL_INTERVAL_FAST
        } else {
            POLL_INTERVAL_SLOW
        });
    }
}

/// Terminate a stale daemon process: TERM first, KILL if it lingers.
#[cfg(unix)]
pub(crate) fn terminate_pid(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    for _ in 0..20 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(not(unix))]
pub(crate) fn terminate_pid(_pid: u32) {}

/// Best-effort cleanup after a failed spawn, so a failed mount leaves nothing
/// behind. If the mount got far enough to register, tear it down; otherwise
/// just remove the empty dest dir `__serve` created before erroring.
fn cleanup_failed(dest: &Path) {
    match lookup_id_for(dest) {
        Ok(Some(_)) => {
            let _ = super::end(dest, true);
        }
        _ => {
            // Only remove if empty — never clobber a dir with user content.
            if let Ok(mut entries) = fs::read_dir(dest) {
                if entries.next().is_none() {
                    let _ = fs::remove_dir(dest);
                }
            }
        }
    }
}

/// Readiness signal for a spawned mount. `serve()` registers the mount in the
/// index *before* the blocking event loop, so registration alone doesn't mean
/// files are being served — confirm the path is an actual mountpoint too.
fn is_ready(dest: &Path) -> Result<bool> {
    if lookup_id_for(dest)?.is_none() {
        return Ok(false);
    }
    Ok(is_mountpoint(dest))
}

/// True once `dest` is a mountpoint distinct from its parent filesystem.
#[cfg(unix)]
pub fn is_mountpoint(dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(here), Some(parent)) = (fs::metadata(dest), dest.parent()) else {
        return false;
    };
    let Ok(up) = fs::metadata(parent) else {
        return false;
    };
    // After FUSE mounts, the mountpoint lives on a different device than the
    // directory it was mounted onto.
    here.dev() != up.dev()
}

/// ProjFS mounts onto a directory on the same NTFS volume, so the device-id
/// trick doesn't apply. Fall back to "registered and the root is readable".
#[cfg(not(unix))]
pub fn is_mountpoint(dest: &Path) -> bool {
    fs::read_dir(dest).is_ok()
}

/// Detach the spawned mount into its own session so it outlives the launching
/// process and doesn't catch a SIGHUP when that process group goes away.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and the only thing we run in the
    // forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach(_cmd: &mut std::process::Command) {}

/// A per-destination log file in the system temp dir for the detached mount.
fn log_path_for(dest: &Path) -> PathBuf {
    let slug: String = dest
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::env::temp_dir().join(format!("oak-mount-{slug}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_is_sanitized_and_under_temp() {
        let p = log_path_for(Path::new("/work/oaktree/oak/oak"));
        assert!(p.starts_with(std::env::temp_dir()));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let stem = name.strip_suffix(".log").unwrap();
        assert!(stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert!(stem.contains("oak"));
    }

    #[cfg(unix)]
    #[test]
    fn plain_directory_is_not_a_mountpoint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("sub");
        std::fs::create_dir(&dir).unwrap();
        // A freshly created dir shares its parent's device, so it must not
        // read as a live mountpoint.
        assert!(!is_mountpoint(&dir));
    }
}
