//! `WorktreeCreate` / `WorktreeRemove` hook handlers — non-git VCS
//! integration for Claude Code.
//!
//! Claude Code's `--worktree` flag and subagent `isolation: "worktree"`
//! delegate worktree creation and cleanup to user-configured hooks when the
//! default git behavior doesn't fit. An Oak space wires these two
//! subcommands into its `.claude/settings.json`, so an isolated session gets
//! its own `oak mount` — a lazy FUSE view on its own virtual branch —
//! instead of a `git worktree`, and `oak mount end` tears it down on cleanup.
//!
//! Protocol (<https://code.claude.com/docs/en/hooks#worktreecreate>):
//!   - Both hooks receive a JSON object on stdin.
//!   - `WorktreeCreate` MUST print the worktree path on stdout and exit 0 on
//!     success; *any* non-zero exit aborts worktree creation. So this
//!     handler must keep stdout clean (only the path) and surface failures
//!     as a non-zero exit.
//!   - `WorktreeRemove` cannot block creation/removal; its failures are
//!     advisory only, so it always reports success.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;

use oak_core::{OakError, Result};

use super::state::lookup_id_for;
use crate::output;

/// How long to wait for a freshly spawned mount to become a live mountpoint
/// before giving up. Fetching the manifest + blob sizes on first mount can
/// take a few seconds on a large repo, so this is generous.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Deserialize)]
struct WorktreeCreateInput {
    worktree_path: PathBuf,
    /// The base ref Claude Code wants the worktree branched from (a *git*
    /// ref, e.g. `origin/HEAD`). Oak resolves the repo's own default branch
    /// at mount time, so we accept and ignore it.
    #[serde(default)]
    #[allow(dead_code)]
    base_ref: Option<String>,
}

#[derive(Deserialize)]
struct WorktreeRemoveInput {
    worktree_path: PathBuf,
}

fn read_stdin_json<T: DeserializeOwned>() -> Result<T> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(OakError::Io)?;
    serde_json::from_str(&buf)
        .map_err(|e| OakError::Server(format!("invalid worktree hook input JSON: {e}")))
}

/// Resolve a (possibly relative) hook-supplied path against the current
/// directory. Claude Code passes an absolute `worktree_path` today, but
/// don't assume it.
fn abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// `oak mount worktree-create` — the `WorktreeCreate` hook.
///
/// Spawns a detached `oak mount start <spec> <worktree_path>`, waits until
/// the kernel reports the path as a live mountpoint, then prints the path on
/// stdout. `oak mount start` runs the FUSE event loop in the foreground for
/// the life of the mount, so it can't run inline — the hook has to return
/// while the mount keeps serving.
pub fn worktree_create(remote: &str, spec: &str) -> Result<()> {
    let input: WorktreeCreateInput = read_stdin_json()?;
    let dest = abs(&input.worktree_path);

    // Idempotent: if a mount is already registered here, report success.
    if lookup_id_for(&dest)?.is_some() {
        println!("{}", dest.display());
        return Ok(());
    }

    // Tee the detached mount's chatter to a log so a failed mount has a
    // diagnosable trail (its stdout/stderr would otherwise be lost — and we
    // must keep *this* process's stdout clean for the path).
    let log_path = log_path_for(&dest);
    let log = fs::File::create(&log_path)?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe().map_err(OakError::Io)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mount")
        .arg("start")
        .arg("--remote")
        .arg(remote)
        .arg(spec)
        .arg(&dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    detach(&mut cmd);

    let mut child = cmd.spawn().map_err(OakError::Io)?;

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if is_ready(&dest)? {
            println!("{}", dest.display());
            return Ok(());
        }
        // If the mount process died before becoming ready, surface its log
        // rather than spinning until the timeout.
        if let Some(status) = child.try_wait().map_err(OakError::Io)? {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            cleanup_failed(&dest);
            return Err(OakError::Server(format!(
                "`oak mount start {spec} {}` exited ({status}) before the worktree was ready:\n{}",
                dest.display(),
                log.trim()
            )));
        }
        if Instant::now() >= deadline {
            // Stop the still-spawning mount and leave nothing half-created.
            let _ = child.kill();
            cleanup_failed(&dest);
            return Err(OakError::Server(format!(
                "timed out after {}s waiting for the mount at {} to come up",
                READY_TIMEOUT.as_secs(),
                dest.display()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Best-effort cleanup after a failed `worktree_create`, so a failed mount
/// leaves nothing behind (matching git's clean-on-failure behavior). If the
/// mount got far enough to register, tear it down; otherwise just remove the
/// empty dest dir `oak mount start` created before erroring.
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

/// `oak mount worktree-remove` — the `WorktreeRemove` hook.
///
/// By the time Claude Code fires this, it has already decided to discard the
/// worktree (matching git's "remove" path — automatic only when there are no
/// changes, otherwise after an explicit prompt). So we force-tear down even a
/// dirty overlay. `WorktreeRemove` failures can't block removal, so any error
/// is logged and we still report success.
pub fn worktree_remove() -> Result<()> {
    let input: WorktreeRemoveInput = read_stdin_json()?;
    let dest = abs(&input.worktree_path);

    if lookup_id_for(&dest)?.is_none() {
        // Nothing of ours registered here — leave it for git/other handlers.
        return Ok(());
    }

    if let Err(e) = super::end(&dest, true) {
        output::warning(&format!(
            "oak mount end failed for {}: {e} (worktree removal continues)",
            dest.display()
        ));
    }
    Ok(())
}

/// Readiness signal for a worktree mount. `start()` registers the mount in
/// the index *before* the blocking FUSE loop, so registration alone doesn't
/// mean files are being served — confirm the path is an actual mountpoint
/// too.
fn is_ready(dest: &Path) -> Result<bool> {
    if lookup_id_for(dest)?.is_none() {
        return Ok(false);
    }
    Ok(is_mountpoint(dest))
}

/// True once `dest` is a mountpoint distinct from its parent filesystem.
#[cfg(unix)]
fn is_mountpoint(dest: &Path) -> bool {
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
fn is_mountpoint(dest: &Path) -> bool {
    fs::read_dir(dest).is_ok()
}

/// Detach the spawned mount into its own session so it outlives this hook
/// process and doesn't catch a SIGHUP when the hook's process group goes
/// away.
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
    std::env::temp_dir().join(format!("oak-worktree-{slug}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_create_input() {
        let json = r#"{
            "session_id": "s1",
            "transcript_path": "/tmp/t.json",
            "cwd": "/work",
            "hook_event_name": "WorktreeCreate",
            "worktree_path": "/work/.claude/worktrees/feature-x",
            "base_ref": "origin/HEAD"
        }"#;
        let input: WorktreeCreateInput = serde_json::from_str(json).unwrap();
        assert_eq!(
            input.worktree_path,
            PathBuf::from("/work/.claude/worktrees/feature-x")
        );
    }

    #[test]
    fn worktree_create_input_tolerates_missing_base_ref() {
        // `base_ref` is optional on our side — Oak resolves the default
        // branch itself.
        let json = r#"{ "worktree_path": "/w/x" }"#;
        let input: WorktreeCreateInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.worktree_path, PathBuf::from("/w/x"));
    }

    #[test]
    fn log_path_is_sanitized_and_under_temp() {
        let p = log_path_for(Path::new("/work/.claude/worktrees/feat-1"));
        assert!(p.starts_with(std::env::temp_dir()));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        // Only the `.log` suffix should contain non-alphanumeric/dash chars.
        let stem = name.strip_suffix(".log").unwrap();
        assert!(stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert!(stem.contains("feat-1"));
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
