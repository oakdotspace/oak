//! `oak watch hook <event>` — the Claude Code hook entrypoint.
//!
//! Wired (by `oak skill install` / `oak init`) for four events:
//!
//! - `SessionStart` — make sure the watch daemon runs (when enabled and on a
//!   task branch), and brief the agent on the workflow.
//! - `UserPromptSubmit` / `PostToolUse` — deliver notices the daemon queued
//!   since the last hook (main merged in, files changed, CONFLICT with the
//!   exact resolve commands, push failures). A still-unresolved conflict is
//!   repeated on every prompt.
//! - `SessionEnd` — stop the daemon (it marks the branch idle on its way
//!   out), unless another session is still active in this checkout.
//!
//! Every event also records the session's activity (see
//! [`super::state::WatchPaths::touch_session`]) and revives a daemon that
//! stopped for lack of it.
//!
//! Output is Claude Code's `hookSpecificOutput.additionalContext` JSON, or
//! nothing. The hook must be fast and must never break the agent: it never
//! touches the network (SessionStart only spawns the daemon), never waits on
//! the daemon, and always exits 0 — outside a watchable checkout it is
//! silent.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use oak_core::Result;
use serde_json::json;

use super::state::{WatchState, IDLE_STOP_REASON};
use crate::output;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    SessionEnd,
}

impl Event {
    fn parse(raw: &str) -> Option<Self> {
        let normalized: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match normalized.as_str() {
            "sessionstart" => Some(Event::SessionStart),
            "userpromptsubmit" => Some(Event::UserPromptSubmit),
            "posttooluse" => Some(Event::PostToolUse),
            "sessionend" => Some(Event::SessionEnd),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PostToolUse => "PostToolUse",
            Event::SessionEnd => "SessionEnd",
        }
    }
}

const WORKFLOW_BRIEF: &str = "This is an Oak repository (use `oak`, never `git`). Start every task with `oak start \"<one-line task summary>\"`: it creates and pushes a branch for the task and runs `oak watch`, which keeps the branch merged with main and pushes each `oak commit`. If oak watch reports a CONFLICT, stop and resolve it immediately (`oak conflict status`, edit the files, `oak pull --continue`). End the task with `oak finish --desc-file <file> --json`.";

/// Run the hook for `event`. Errors are swallowed: a hook failure must never
/// surface to the agent.
pub async fn run(event: &str, cwd: &Path) -> Result<()> {
    let Some(event) = Event::parse(event) else {
        return Ok(());
    };
    let input = read_input();
    let dir = input
        .as_ref()
        .and_then(|value| value.get("cwd"))
        .and_then(|cwd| cwd.as_str())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| cwd.to_path_buf());
    let session = input
        .as_ref()
        .and_then(|value| value.get("session_id"))
        .and_then(|id| id.as_str());
    if let Some(context) = context_for(event, &dir, session).await {
        let payload = json!({
            "hookSpecificOutput": {
                "hookEventName": event.name(),
                "additionalContext": context,
            }
        });
        output::print_line(&payload.to_string());
    }
    Ok(())
}

/// Claude Code passes the hook input as JSON on stdin. Never block on an
/// interactive terminal (someone running the hook by hand).
fn read_input() -> Option<serde_json::Value> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut text = String::new();
    stdin
        .lock()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

async fn context_for(event: Event, dir: &Path, session: Option<&str>) -> Option<String> {
    let target = super::target(dir).ok()?;
    if let Some(session) = session {
        if event == Event::SessionEnd {
            target.paths.remove_session(session);
        } else {
            target.paths.touch_session(session);
        }
    }
    match event {
        Event::SessionEnd => {
            // Signal and return; the daemon sends idle itself. No waiting,
            // no network here. Another agent in this checkout keeps it alive.
            let window = super::daemon::idle_timeout()
                .unwrap_or(super::daemon::DEFAULT_IDLE_TIMEOUT)
                .as_secs();
            if target.paths.daemon_running() && !target.paths.any_session_live(window) {
                super::stop_target(&target, false, false).await;
            }
            None
        }
        Event::SessionStart => {
            let mut parts = vec![WORKFLOW_BRIEF.to_string()];
            let settings = crate::commands::config::resolve_for_path(&target.work_tree);
            if settings.enabled.value {
                if let Ok(report) = super::ensure_running(&target, false) {
                    parts.push(format!(
                        "oak watch is {} for branch '{}'.",
                        if report.already_running {
                            "running"
                        } else {
                            "starting"
                        },
                        report.branch
                    ));
                }
            } else {
                parts.push(format!(
                    "oak watch is disabled here ({}), so pull and push yourself.",
                    settings.enabled.source.describe()
                ));
            }
            parts.extend(deliver(&target, true));
            Some(parts.join("\n\n"))
        }
        Event::UserPromptSubmit => {
            resume_after_idle(&target);
            join(deliver(&target, true))
        }
        Event::PostToolUse => {
            resume_after_idle(&target);
            join(deliver(&target, false))
        }
    }
}

/// An agent is back on a checkout whose daemon gave up on it. A daemon that
/// was stopped on purpose stays stopped.
fn resume_after_idle(target: &super::WatchTarget) {
    let idled = target
        .paths
        .load_state()
        .is_some_and(|state| state.stop_reason.as_deref() == Some(IDLE_STOP_REASON));
    if idled && !target.paths.daemon_running() {
        let _ = super::ensure_running(target, false);
    }
}

fn join(parts: Vec<String>) -> Option<String> {
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Unseen notices (advancing the cursor), plus — when `remind` — a reminder
/// of a conflict that is still unresolved but was already delivered.
fn deliver(target: &super::WatchTarget, remind: bool) -> Vec<String> {
    let Some(state) = target.paths.load_state() else {
        return Vec::new();
    };
    let cursor = target.paths.load_cursor();
    let unseen = super::unseen(&state, cursor);
    let mut parts: Vec<String> = unseen.iter().map(|n| n.message.clone()).collect();
    if let Some(last) = unseen.last() {
        target.paths.save_cursor(last.id);
    }
    let delivered_conflict = unseen.iter().any(|n| n.kind == "conflict");
    if remind && !delivered_conflict {
        if let Some(reminder) = conflict_reminder(&state, &target.oak_dir) {
            parts.push(reminder);
        }
    }
    parts
}

fn conflict_reminder(state: &WatchState, oak_dir: &Path) -> Option<String> {
    let conflict = state.conflict.as_ref()?;
    if !oak_dir.join("SYNC_HEAD").exists() {
        return None;
    }
    Some(format!(
        "Reminder: the merge of main that oak watch started is still conflicted{}. Resolve it before anything else: `oak conflict status`, fix the files, `oak pull --continue`.",
        if conflict.paths.is_empty() {
            String::new()
        } else {
            format!(" ({})", conflict.paths.join(", "))
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_parse_in_any_spelling() {
        assert_eq!(Event::parse("SessionStart"), Some(Event::SessionStart));
        assert_eq!(Event::parse("session-start"), Some(Event::SessionStart));
        assert_eq!(Event::parse("post_tool_use"), Some(Event::PostToolUse));
        assert_eq!(
            Event::parse("UserPromptSubmit"),
            Some(Event::UserPromptSubmit)
        );
        assert_eq!(Event::parse("SessionEnd"), Some(Event::SessionEnd));
        assert_eq!(Event::parse("Stop"), None);
    }

    #[test]
    fn notices_are_delivered_once_and_conflicts_are_repeated() {
        let tmp = tempfile::tempdir().unwrap();
        let oak_dir = tmp.path().join(".oak");
        std::fs::create_dir_all(&oak_dir).unwrap();
        let target = super::super::WatchTarget {
            work_tree: tmp.path().to_path_buf(),
            paths: super::super::state::WatchPaths::new(&oak_dir),
            oak_dir: oak_dir.clone(),
        };
        let mut state = WatchState::default();
        state.push_notice("main_merged", "merged".to_string());
        state.push_notice("conflict", "CONFLICT".to_string());
        state.conflict = Some(super::super::state::PendingConflict {
            main_head: Some("abc".to_string()),
            paths: vec!["a.txt".to_string()],
            at: 0,
        });
        target.paths.save_state(&state).unwrap();
        std::fs::write(oak_dir.join("SYNC_HEAD"), "x").unwrap();

        assert_eq!(deliver(&target, false), vec!["merged", "CONFLICT"]);
        assert!(deliver(&target, false).is_empty());
        let reminder = deliver(&target, true);
        assert_eq!(reminder.len(), 1);
        assert!(reminder[0].contains("a.txt"), "{reminder:?}");

        std::fs::remove_file(oak_dir.join("SYNC_HEAD")).unwrap();
        assert!(deliver(&target, true).is_empty());
    }
}
