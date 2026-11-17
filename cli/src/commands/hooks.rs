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
use crate::resolve::RepoContext;

/// Hook directory: `<oak dir>/hooks`. Mirrors git's `.git/hooks` — hooks are
/// local, not versioned alongside the repo, so each developer chooses what
/// runs on their machine.
///
/// The oak dir is `<work tree>/.oak`, which no checkout ever materializes
/// from repository content.
fn hooks_dir(ctx: &RepoContext) -> PathBuf {
    ctx.oak_dir.join("hooks")
}

/// Resolve the executable path for an event. Returns `None` when no hook is
/// installed (the common case). Files ending in `.sample` are intentionally
/// ignored so users can drop disabled templates next to active hooks.
pub fn hook_path(ctx: &RepoContext, event: &str) -> Option<PathBuf> {
    let candidate = hooks_dir(ctx).join(event);
    if candidate.is_file() && is_executable(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// Whether `path`, inside a work tree that also has a `.git`, is tracked by
/// git. A native Oak repo can sit beside a `.git` (`oak init` in a git repo),
/// and a hostile git repository can commit a whole `.oak/` — database and
/// hooks — that a plain `git clone` checks out, so a hook git tracks came
/// from the repository. When git can't answer, the hook is treated as tracked
/// (not run). Paths outside the work tree are never tracked.
fn tracked_by_git(work_tree: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(work_tree) else {
        return false;
    };
    if !work_tree.join(".git").exists() {
        return false;
    }
    match Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args(["ls-files", "--"])
        .arg(relative)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => true,
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
pub fn run_hook(ctx: &RepoContext, event: &str, blocking: bool) -> Result<()> {
    let Some(path) = hook_path(ctx, event) else {
        return Ok(());
    };
    let work_tree = ctx.work_tree.as_path();
    if tracked_by_git(work_tree, &path) {
        output::warning(&format!(
            "Not running {} — git tracks it, so it is repository content, not a local hook.",
            path.display()
        ));
        return Ok(());
    }

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_hook(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn native_repo_beside_git_skips_git_tracked_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let work_tree = dir.path().to_path_buf();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&work_tree)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        let tracked = work_tree.join(".oak/hooks/pre-commit");
        write_hook(&tracked);
        let untracked = work_tree.join(".oak/hooks/post-commit");
        write_hook(&untracked);
        git(&["add", "-f", "--", ".oak/hooks/pre-commit"]);
        assert!(tracked_by_git(&work_tree, &tracked));
        assert!(!tracked_by_git(&work_tree, &untracked));
        // Outside the work tree is never tracked.
        assert!(!tracked_by_git(&work_tree, Path::new("/elsewhere/hook")));
    }

    #[test]
    fn native_repo_runs_dot_oak_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let work_tree = dir.path().to_path_buf();
        let ctx = RepoContext {
            oak_dir: work_tree.join(".oak"),
            work_tree: work_tree.clone(),
        };
        write_hook(&work_tree.join(".oak/hooks/pre-commit"));
        assert_eq!(
            hook_path(&ctx, "pre-commit"),
            Some(work_tree.join(".oak/hooks/pre-commit"))
        );
    }
}
