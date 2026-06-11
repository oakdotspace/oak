//! `WorktreeCreate` / `WorktreeRemove` hook handlers — non-git VCS
//! integration for Claude Code.
//!
//! Claude Code's `--worktree` flag and subagent `isolation: "worktree"`
//! delegate worktree creation and cleanup to user-configured hooks when the
//! default git behavior doesn't fit. Wiring these two subcommands into a
//! project's `.claude/settings.json` gives an isolated session its own
//! `oak mount` — a lazy FUSE view on its own virtual branch — instead of a
//! `git worktree`, and `oak mount end` tears it down on cleanup.
//!
//! Protocol (<https://code.claude.com/docs/en/hooks#worktreecreate>):
//!   - Both hooks receive a JSON object on stdin.
//!   - `WorktreeCreate` MUST print the worktree path on stdout and exit 0 on
//!     success; *any* non-zero exit aborts worktree creation. So this
//!     handler must keep stdout clean (only the path) and surface failures
//!     as a non-zero exit.
//!   - `WorktreeRemove` cannot block creation/removal; its failures are
//!     advisory only, so it always reports success.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;

use oak_core::{OakError, Result};

use super::spawn::spawn_detached;
use super::state::lookup_id_for;
use crate::output;

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
/// Spawns a detached background mount at the worktree path, waits until it's a
/// live mountpoint, then prints the path on stdout. The heavy lifting (spawn,
/// readiness polling, cleanup-on-failure) lives in [`spawn_detached`].
pub fn worktree_create(remote: &str, spec: &str) -> Result<()> {
    let input: WorktreeCreateInput = read_stdin_json()?;
    let dest = abs(&input.worktree_path);
    spawn_detached(remote, spec, &dest)?;
    println!("{}", dest.display());
    Ok(())
}

/// `oak mount worktree-remove` — the `WorktreeRemove` hook.
///
/// `WorktreeRemove` failures can't block removal, so any error is logged and
/// we still report success. The actual policy lives in [`worktree_remove_at`].
pub fn worktree_remove() -> Result<()> {
    let input: WorktreeRemoveInput = read_stdin_json()?;
    worktree_remove_at(&abs(&input.worktree_path))
}

/// Tear down the mount at `dest` for the `WorktreeRemove` hook — but only
/// when that loses nothing. The hook fires on routine session cleanup, not
/// just deliberate discards, so it must never force: a dirty overlay or
/// committed-but-unpushed commits mean the mount's state dir holds the only
/// copy of the agent's work, and we leave the mount in place (reporting why)
/// rather than destroy it. The user can finish with `oak push` + `oak mount
/// end`, or discard explicitly with `oak mount end --force`.
pub fn worktree_remove_at(dest: &Path) -> Result<()> {
    let Some(id) = lookup_id_for(dest)? else {
        // Nothing of ours registered here — leave it for git/other handlers.
        return Ok(());
    };
    let state_dir = super::state::state_dir_for(&id)?;

    let overlay = super::state::load_overlay_meta(&state_dir).unwrap_or_default();
    let dirty =
        !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty();
    let unpushed = super::unpushed_commits(&state_dir);
    if dirty || unpushed > 0 {
        output::warning(&format!(
            "leaving mount {} in place: {} (run `oak push` inside it, then `oak mount end`; \
             or `oak mount end --force` to discard)",
            dest.display(),
            if dirty {
                "it has uncommitted changes".to_string()
            } else {
                format!("it has {unpushed} unpushed commit(s)")
            },
        ));
        return Ok(());
    }

    if let Err(e) = super::end(dest, false) {
        output::warning(&format!(
            "oak mount end failed for {}: {e} (worktree removal continues)",
            dest.display()
        ));
    }
    Ok(())
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
}
