//! `oak skill` — install the bundled agent skill so coding agents know how to
//! drive Oak.
//!
//! Oak ships an "agent skill": a `SKILL.md` plus reference files in the open
//! Agent Skills format that Claude Code (and other skill-aware agents)
//! auto-discover from `.claude/skills/`. The canonical copy lives in
//! [`cli/src/commands/skill/oak-vcs/`](.) and is `include_str!`'d into the
//! binary — the same pattern as the space templates — so an installed skill
//! always documents exactly the CLI version that wrote it, and re-running
//! `oak skill install` after `oak upgrade` brings the docs along.
//!
//! `oak skill install` writes the skill into the current repo's
//! `.claude/skills/oak-vcs/` — project-level, committed with the repo so every
//! collaborator's agent picks it up. `--global` targets
//! `~/.claude/skills/oak-vcs/` instead, covering every project on the machine
//! (including mounts and spaces, whose directories are created on the fly).
//!
//! Unlike the space scaffolding, installation *overwrites* stale copies: these
//! files are documentation of the CLI itself, managed by `oak`, and drifting
//! out of date is worse than stomping local edits (which belong upstream in
//! this directory instead). Unchanged files are left untouched so re-installs
//! are cheap and quiet.

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
        "references/mounts-and-spaces.md",
        include_str!("oak-vcs/references/mounts-and-spaces.md"),
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
    output::info(
        "Claude Code picks it up automatically. For agents that only read AGENTS.md, add a line \
         like: \"Read .claude/skills/oak-vcs/SKILL.md before using version control here.\"",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Make the temp dir a minimal Oak repo so project-level resolution works.
        fs::create_dir_all(tmp.path().join(".oak")).unwrap();

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
