//! Local commit hooks (Git-style).
//!
//! Hooks live at `.oak/hooks/<event>` and are spawned as child processes at
//! well-defined points in the lifecycle of a command. A non-zero exit from a
//! blocking hook (currently `pre-commit`) aborts the command before any
//! repository state mutates; non-blocking hooks (`post-commit`) only print a
//! warning. Hooks are intentionally local-only — they're not pushed with
//! commits — because the same script that auto-fixes things on a developer's
//! machine (running `cargo fmt`) is rarely what should run inside a CI sandbox.
//!
//! The `cargo` template installs a `pre-commit` that auto-formats Rust code
//! and fails on clippy warnings, so the user's CI's `cargo fmt --check`
//! never trips on something a 5-second local format would have fixed.

use std::path::{Path, PathBuf};
use std::process::Command;

use oak_core::{OakError, Result};

use crate::output;

/// Events Oak knows how to fire. The order matters for `list`; new events
/// added here automatically show up in the listing.
pub const SUPPORTED_EVENTS: &[&str] = &["pre-commit", "post-commit"];

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

/// Run the hook for `event` if one is installed. Streams the hook's stdout/
/// stderr to the user's terminal so failure output is immediately visible —
/// the whole point is for the developer to see what `cargo clippy` (or
/// whatever) said.
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

// ---------------------------------------------------------------------------
// CLI subcommands: list / install / edit / remove / path
// ---------------------------------------------------------------------------

/// `oak hooks list` — print every supported event with its current state
/// (installed / not installed / file present but not executable).
pub fn list(cwd: &Path) -> Result<()> {
    let work_tree = crate::resolve::resolve(cwd)?.work_tree;
    use output::colors::*;

    output::header("Hooks");
    let dir = hooks_dir(&work_tree);
    output::detail("Directory", &dir.display().to_string());
    output::blank();

    for event in SUPPORTED_EVENTS {
        let path = dir.join(event);
        let (status_label, status_color) = if !path.exists() {
            ("not installed", DIM)
        } else if !is_executable(&path) {
            ("present (not executable)", YELLOW)
        } else {
            ("installed", GREEN)
        };
        output::print_line(&format!(
            "  {CYAN}{event:<14}{RESET} {status_color}{status_label}{RESET}"
        ));
    }

    output::blank();
    output::print_line("  Install a template:  oak hooks install cargo");
    output::print_line("  Edit a hook:         oak hooks edit pre-commit");
    Ok(())
}

/// Install a template. The only built-in template today is `cargo`, which
/// runs `cargo fmt --all` (auto-fix) and `cargo clippy --all-targets -- -D
/// warnings` (block) as `pre-commit`. Falls through gracefully when there's
/// no Cargo.toml so the same script can live in a non-Rust repo.
pub fn install(cwd: &Path, template: &str) -> Result<()> {
    let work_tree = crate::resolve::resolve(cwd)?.work_tree;
    let dir = hooks_dir(&work_tree);
    std::fs::create_dir_all(&dir)?;

    match template {
        "cargo" => install_cargo(&dir)?,
        other => {
            return Err(OakError::Config(format!(
                "Unknown hook template '{other}'. Available: cargo"
            )));
        }
    }
    Ok(())
}

fn install_cargo(dir: &Path) -> Result<()> {
    let target = dir.join("pre-commit");
    if target.exists() {
        return Err(OakError::Config(format!(
            "{} already exists. Remove it first with `oak hooks remove pre-commit`.",
            target.display()
        )));
    }

    std::fs::write(&target, CARGO_PRE_COMMIT)?;
    set_executable(&target)?;

    output::success(&format!(
        "Installed pre-commit hook at {}",
        target.display()
    ));
    output::item("Runs: cargo fmt --all && cargo clippy --all-targets -- -D warnings");
    output::item("Skip with: oak commit --no-verify");
    Ok(())
}

const CARGO_PRE_COMMIT: &str = "\
#!/usr/bin/env bash
# Oak pre-commit hook installed by `oak hooks install cargo`.
#
# Auto-formats Rust code (so the commit always lands clean) and fails the
# commit if `cargo clippy` has any warnings. Bypass with:
#   oak commit --no-verify
set -euo pipefail

# Skip cleanly if this isn't a Cargo workspace — same hook can live in
# a parent dir alongside non-Rust subprojects.
if [ ! -f Cargo.toml ]; then
    exit 0
fi

echo \"pre-commit: cargo fmt --all\"
cargo fmt --all

echo \"pre-commit: cargo clippy --all-targets -- -D warnings\"
cargo clippy --all-targets -- -D warnings
";

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// `oak hooks edit <event>` — open the hook in `$EDITOR`, falling back to
/// `vi`. Creates an empty stub with shebang if the file doesn't exist yet so
/// the editor opens onto something usable.
pub fn edit(cwd: &Path, event: &str) -> Result<()> {
    validate_event(event)?;
    let work_tree = crate::resolve::resolve(cwd)?.work_tree;
    let dir = hooks_dir(&work_tree);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(event);

    if !path.exists() {
        std::fs::write(
            &path,
            "#!/usr/bin/env bash\n# Oak hook. Exit 0 to allow the operation, non-zero to abort.\nset -euo pipefail\n",
        )?;
        set_executable(&path)?;
    }

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|e| OakError::Config(format!("Failed to launch editor '{editor}': {e}")))?;

    if !status.success() {
        return Err(OakError::Config(format!(
            "Editor '{editor}' exited with {status}"
        )));
    }
    Ok(())
}

/// `oak hooks remove <event>` — delete the hook file. No-op if it doesn't
/// exist; that way the command is idempotent in scripts.
pub fn remove(cwd: &Path, event: &str) -> Result<()> {
    validate_event(event)?;
    let work_tree = crate::resolve::resolve(cwd)?.work_tree;
    let path = hooks_dir(&work_tree).join(event);
    if !path.exists() {
        output::info(&format!("No {event} hook installed"));
        return Ok(());
    }
    std::fs::remove_file(&path)?;
    output::success(&format!("Removed {event} hook"));
    Ok(())
}

/// `oak hooks path <event>` — print the resolved file path. Useful for
/// scripting (`$(oak hooks path pre-commit)`).
pub fn print_path(cwd: &Path, event: &str) -> Result<()> {
    validate_event(event)?;
    let work_tree = crate::resolve::resolve(cwd)?.work_tree;
    let path = hooks_dir(&work_tree).join(event);
    output::print_line(&path.display().to_string());
    Ok(())
}

fn validate_event(event: &str) -> Result<()> {
    if SUPPORTED_EVENTS.contains(&event) {
        Ok(())
    } else {
        Err(OakError::Config(format!(
            "Unknown hook event '{event}'. Supported: {}",
            SUPPORTED_EVENTS.join(", ")
        )))
    }
}
