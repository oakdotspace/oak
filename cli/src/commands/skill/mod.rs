//! `oak skill` — install the bundled agent skill so coding agents know how to
//! drive Oak.
//!
//! Oak ships an "agent skill": a `SKILL.md` plus reference files in the open
//! Agent Skills format that Claude Code (and other skill-aware agents)
//! auto-discover from `.claude/skills/`. The canonical copy lives in
//! [`cli/src/commands/skill/oak-vcs/`](.) and is `include_str!`'d into the
//! binary, so an installed skill
//! always documents exactly the CLI version that wrote it, and re-running
//! `oak skill install` after `oak upgrade` brings the docs along.
//!
//! `oak skill install` writes the skill into the current repo's
//! `.claude/skills/oak-vcs/` — project-level, committed with the repo so every
//! collaborator's agent picks it up. `--global` targets
//! `~/.claude/skills/oak-vcs/` instead, covering every project on the machine
//! (including mounts, whose directories are created on the fly).
//!
//! Installation *overwrites* stale copies: these
//! files are documentation of the CLI itself, managed by `oak`, and drifting
//! out of date is worse than stomping local edits (which belong upstream in
//! this directory instead). Unchanged files are left untouched so re-installs
//! are cheap and quiet.
//!
//! Install also wires the `oak watch` Claude Code hooks into the matching
//! `settings.json` (`.claude/settings.json`, or `~/.claude/settings.json`
//! with `--global`) — see [`install_claude_hooks`]. Settings are a user file,
//! so that step *merges*: existing keys and hooks are kept, and an Oak hook
//! already present is not added twice.

use std::fs;
use std::path::Path;

use oak_core::{OakError, Result};

use crate::output;

/// Directory name of the skill, under `.claude/skills/`.
const SKILL_NAME: &str = "oak-vcs";

/// Every file the skill ships, as `(relative path, contents)`. The eval suite
/// next to the canonical copy is a dev artifact and deliberately not listed.
const SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("oak-vcs/SKILL.md")),
    (
        "references/commands.md",
        include_str!("oak-vcs/references/commands.md"),
    ),
    (
        "references/mounts.md",
        include_str!("oak-vcs/references/mounts.md"),
    ),
    (
        "references/ci-and-merging.md",
        include_str!("oak-vcs/references/ci-and-merging.md"),
    ),
    (
        "references/json-and-automation.md",
        include_str!("oak-vcs/references/json-and-automation.md"),
    ),
    (
        "references/conflicts-and-recovery.md",
        include_str!("oak-vcs/references/conflicts-and-recovery.md"),
    ),
];

/// `oak skill install [--global]` — write the bundled skill to disk.
///
/// Project-level by default: resolves the enclosing repository the same way
/// every other command does (so it works from any subdirectory, and inside a
/// mount) and writes under `<work tree>/.claude/skills/`. With `global`,
/// writes under `~/.claude/skills/` and needs no repository at all.
pub fn install(global: bool, cwd: &Path) -> Result<()> {
    let (skills_root, scope) = if global {
        let home = dirs::home_dir().ok_or_else(|| {
            OakError::InvalidArgument("could not determine your home directory".to_string())
        })?;
        (
            home.join(".claude").join("skills"),
            "all projects (user-wide)".to_string(),
        )
    } else {
        let ctx = crate::resolve::resolve(cwd).map_err(|e| match e {
            OakError::RepoNotFound => OakError::InvalidArgument(
                "not inside a repository — run this from a repo to install the skill \
                 project-level, or pass --global to install it user-wide (~/.claude/skills)"
                    .to_string(),
            ),
            other => other,
        })?;
        (
            ctx.work_tree.join(".claude").join("skills"),
            format!("this repo ({})", ctx.work_tree.display()),
        )
    };

    let dir = skills_root.join(SKILL_NAME);
    let mut written = 0usize;
    let mut unchanged = 0usize;
    for (rel, contents) in SKILL_FILES {
        let path = dir.join(rel);
        if fs::read_to_string(&path).is_ok_and(|cur| cur == *contents) {
            unchanged += 1;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, contents)?;
        written += 1;
    }

    if written == 0 {
        output::success(&format!(
            "Skill {SKILL_NAME} already up to date at {} ({unchanged} files).",
            dir.display()
        ));
    } else {
        output::success(&format!(
            "Installed skill {SKILL_NAME} for {scope}: {written} file(s) written, \
             {unchanged} already current, at {}.",
            dir.display()
        ));
    }
    let settings_path = skills_root
        .parent()
        .map(|claude_dir| claude_dir.join("settings.json"))
        .ok_or_else(|| OakError::InvalidArgument("invalid skills directory".to_string()))?;
    report_hooks_install(&settings_path, install_claude_hooks(&settings_path));
    output::info(
        "Claude Code picks it up automatically. For agents that only read AGENTS.md, add a line \
         like: \"Read .claude/skills/oak-vcs/SKILL.md before using version control here.\"",
    );
    Ok(())
}

/// Claude Code events wired to `oak watch hook <event>`.
const WATCH_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "SessionEnd",
];
/// Marker identifying an Oak-installed hook command.
const WATCH_HOOK_COMMAND: &str = "oak watch hook";

/// The command wired for `event`. Hooks can be committed to a repo, and an
/// `oak` too old to know `watch` exits 2 on the unknown subcommand, which
/// Claude Code reads as "block this prompt", so failure is swallowed here.
fn watch_hook_command(event: &str) -> String {
    format!("{WATCH_HOOK_COMMAND} {event} || true")
}

/// What [`install_claude_hooks`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum HooksInstall {
    /// Added (or upgraded) hooks for these events.
    Added(Vec<&'static str>),
    /// Every event already runs `oak watch hook`.
    AlreadyPresent,
}

/// Merge the `oak watch` hooks into the Claude Code settings file at `path`,
/// creating it if needed. Keeps every existing key and hook; an event that
/// already runs an `oak watch hook` command is left alone, except that the
/// bare command older versions wrote is upgraded to [`watch_hook_command`].
/// A file that is not a JSON object is refused rather than rewritten.
pub fn install_claude_hooks(path: &Path) -> Result<HooksInstall> {
    use serde_json::{json, Map, Value};

    let mut settings = match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Value::Object(Map::new()),
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| {
            OakError::InvalidArgument(format!(
                "{} is not valid JSON ({e}); fix it, then re-run to add the oak watch hooks",
                path.display()
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e.into()),
    };
    let Some(root) = settings.as_object_mut() else {
        return Err(OakError::InvalidArgument(format!(
            "{} is not a JSON object; not adding the oak watch hooks",
            path.display()
        )));
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(OakError::InvalidArgument(format!(
            "\"hooks\" in {} is not an object; not adding the oak watch hooks",
            path.display()
        )));
    };
    let mut added = Vec::new();
    for event in WATCH_HOOK_EVENTS {
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let legacy = format!("{WATCH_HOOK_COMMAND} {event}");
        let mut present = false;
        let mut upgraded = false;
        let commands = groups
            .iter_mut()
            .filter_map(|group| group.get_mut("hooks")?.as_array_mut())
            .flatten()
            .filter_map(|hook| hook.get_mut("command"));
        for command in commands {
            let Some(text) = command.as_str() else {
                continue;
            };
            present |= text.contains(WATCH_HOOK_COMMAND);
            if text == legacy {
                *command = Value::String(watch_hook_command(event));
                upgraded = true;
            }
        }
        if upgraded {
            added.push(*event);
        }
        if present {
            continue;
        }
        groups.push(json!({
            "hooks": [{
                "type": "command",
                "command": watch_hook_command(event),
                "timeout": 10,
            }]
        }));
        added.push(*event);
    }
    if added.is_empty() {
        return Ok(HooksInstall::AlreadyPresent);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&settings)
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    fs::write(path, format!("{text}\n"))?;
    Ok(HooksInstall::Added(added))
}

/// Narrate an [`install_claude_hooks`] outcome; a failure is a warning, not
/// an error, since the skill itself installed fine.
pub fn report_hooks_install(path: &Path, outcome: Result<HooksInstall>) {
    match outcome {
        Ok(HooksInstall::Added(events)) => output::success(&format!(
            "Added oak watch hooks ({}) to {}",
            events.join(", "),
            path.display()
        )),
        Ok(HooksInstall::AlreadyPresent) => {
            output::vlog(&format!("oak watch hooks already in {}", path.display()))
        }
        Err(error) => output::warning(&format!("oak watch hooks not installed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_merge_into_existing_settings_without_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"permissions":{"allow":["Bash(ls)"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo bye"}]}],"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .unwrap();

        assert_eq!(
            install_claude_hooks(&path).unwrap(),
            HooksInstall::Added(WATCH_HOOK_EVENTS.to_vec())
        );
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls)");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo bye"
        );
        let start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2, "existing SessionStart hook kept");
        assert_eq!(
            start[1]["hooks"][0]["command"],
            "oak watch hook SessionStart || true"
        );

        assert_eq!(
            install_claude_hooks(&path).unwrap(),
            HooksInstall::AlreadyPresent
        );

        fs::write(&path, "not json").unwrap();
        assert!(install_claude_hooks(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn bare_hooks_from_older_installs_are_upgraded_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(
            &path,
            r#"{"hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"oak watch hook SessionEnd","timeout":3}]}],"PostToolUse":[{"hooks":[{"type":"command","command":"my-wrapper oak watch hook PostToolUse"}]}]}}"#,
        )
        .unwrap();

        let HooksInstall::Added(events) = install_claude_hooks(&path).unwrap() else {
            panic!("expected changes");
        };
        assert!(events.contains(&"SessionEnd"), "{events:?}");
        assert!(!events.contains(&"PostToolUse"), "{events:?}");
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let end = settings["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(end.len(), 1, "upgraded, not duplicated");
        assert_eq!(
            end[0]["hooks"][0]["command"],
            "oak watch hook SessionEnd || true"
        );
        assert_eq!(end[0]["hooks"][0]["timeout"], 3);
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "my-wrapper oak watch hook PostToolUse",
            "a customized command is the user's"
        );
        assert_eq!(
            install_claude_hooks(&path).unwrap(),
            HooksInstall::AlreadyPresent
        );
    }

    /// The frontmatter is the skill's identity: the directory name must match
    /// the `name:` field or skill-aware agents refuse to load it.
    #[test]
    fn skill_md_frontmatter_names_the_skill() {
        let (_, skill_md) = SKILL_FILES
            .iter()
            .find(|(rel, _)| *rel == "SKILL.md")
            .expect("SKILL.md must be bundled");
        assert!(skill_md.starts_with("---\n"), "missing YAML frontmatter");
        assert!(
            skill_md.contains(&format!("name: {SKILL_NAME}")),
            "frontmatter name must match the install directory"
        );
    }

    /// Every reference file SKILL.md points at must actually ship, or agents
    /// following the pointers hit dead links.
    #[test]
    fn skill_md_reference_links_all_ship() {
        let (_, skill_md) = SKILL_FILES
            .iter()
            .find(|(rel, _)| *rel == "SKILL.md")
            .expect("SKILL.md must be bundled");
        for (rel, _) in SKILL_FILES.iter().filter(|(rel, _)| *rel != "SKILL.md") {
            assert!(
                skill_md.contains(rel),
                "SKILL.md should mention bundled file {rel}"
            );
        }
    }

    #[test]
    fn install_writes_all_files_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        // Make the temp dir a minimal Oak repo so project-level resolution
        // works. An empty `.oak/` is not a repository; it needs its database.
        fs::create_dir_all(tmp.path().join(".oak")).unwrap();
        oak_core::SqliteRepository::open(&tmp.path().join(".oak/oak.db")).unwrap();

        install(false, tmp.path()).unwrap();
        let dir = tmp.path().join(".claude").join("skills").join(SKILL_NAME);
        for (rel, contents) in SKILL_FILES {
            let on_disk = fs::read_to_string(dir.join(rel)).unwrap();
            assert_eq!(&on_disk, contents, "{rel} should match the bundled copy");
        }

        // A stale copy gets refreshed on re-install.
        fs::write(dir.join("SKILL.md"), "stale").unwrap();
        install(false, tmp.path()).unwrap();
        let refreshed = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert_ne!(refreshed, "stale", "re-install must refresh stale files");
    }
}
