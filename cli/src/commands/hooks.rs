//! Local commit hooks (Git-style).
//!
//! Hooks live at `.oak/hooks/<event>` and are spawned as child processes at
//! well-defined points in the lifecycle of a command. A non-zero exit from a
//! blocking hook (currently `pre-commit`) aborts the command before any
//! repository state mutates; non-blocking hooks (`post-commit`) only print a
//! warning. Hooks are intentionally local-only — they're not pushed with
//! commits — because the same script that auto-fixes things on a developer's
//! machine (running `cargo fmt`) is rarely what should run inside a CI sandbox.

use std::path::{Path, PathBuf};
use std::process::Command;

use oak_core::{OakError, Result};

use crate::output;

/// Hook directory relative to the work tree. Mirrors git's `.git/hooks` —
/// hooks are local, not versioned alongside the repo, so each developer
/// chooses what runs on their machine.
fn hooks_dir(work_tree: &Path) -> PathBuf {
    work_tree.join(".oak").join("hooks")
}

/// Resolve the executable path for an event. Returns `None` when no hook is
/// installed (the common case). Files ending in `.sample` are intentionally
/// ignored so users can drop disabled templates next to active hooks.
pub fn hook_path(work_tree: &Path, event: &str) -> Option<PathBuf> {
    let candidate = hooks_dir(work_tree).join(event);
    if candidate.is_file() && is_executable(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    // On Windows the OS picks the right interpreter from the file extension
    // (`.exe`, `.cmd`, `.bat`, `.ps1`). We don't gate on the exec bit since
    // NTFS doesn't carry one in the Unix sense.
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("exe") | Some("cmd") | Some("bat") | Some("ps1")
    )
}

/// Run the hook for `event` if one is installed. Streams the hook's output to
/// the user's terminal so failure output is immediately visible — the whole
/// point is for the developer to see what `cargo clippy` (or whatever) said.
/// Like git, the hook's stdout is redirected to our stderr: stdout belongs to
/// the command's own data (`oak commit --json` must stay one JSON document).
///
/// `blocking` controls what happens on a non-zero exit: `true` returns an
/// error so the caller can abort, `false` just warns and proceeds. We use
/// blocking for `pre-commit` and non-blocking for `post-commit`.
pub fn run_hook(work_tree: &Path, event: &str, blocking: bool) -> Result<()> {
    let Some(path) = hook_path(work_tree, event) else {
        return Ok(());
    };

    output::vlog(&format!("hooks: running {event}"));
    output::info(&format!("Running {event} hook"));

    let status = Command::new(&path)
        .current_dir(work_tree)
        .env("OAK_HOOK", event)
        .stdout(std::io::stderr())
        .status()
        .map_err(|e| {
            OakError::Config(format!(
                "Failed to spawn {} hook at {}: {e}",
                event,
                path.display()
            ))
        })?;

    if status.success() {
        return Ok(());
    }

    let code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let msg = format!("{event} hook failed (exit {code}). Run `oak commit --no-verify` to skip.");
    if blocking {
        Err(OakError::Config(msg))
    } else {
        output::warning(&msg);
        Ok(())
    }
}
