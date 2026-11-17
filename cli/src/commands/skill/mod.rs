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
//! `~/.claude/skills/oak-vcs/` instead, covering every project on the machine.
//!
//! Installation *overwrites* stale copies: these
//! files are documentation of the CLI itself, managed by `oak`, and drifting
//! out of date is worse than stomping local edits (which belong upstream in
//! this directory instead). Unchanged files are left untouched so re-installs
//! are cheap and quiet.

use std::fs;
use std::path::{Path, PathBuf};

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
        "references/merging.md",
        include_str!("oak-vcs/references/merging.md"),
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

/// Where a copy of the skill lives: in this repo, or user-wide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    Project,
    Global,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }

    /// The command that writes (or refreshes) a copy in this scope.
    fn install_command(self) -> &'static str {
        match self {
            Scope::Project => "oak skill install",
            Scope::Global => "oak skill install --global",
        }
    }
}

/// `~/.claude/skills`, the user-wide skills root.
fn global_skills_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        OakError::InvalidArgument("could not determine your home directory".to_string())
    })?;
    Ok(home.join(".claude").join("skills"))
}

/// `<work tree>/.claude/skills` for the repository enclosing `cwd`.
fn project_skills_root(cwd: &Path) -> Result<PathBuf> {
    let ctx = crate::resolve::resolve(cwd).map_err(|e| match e {
        OakError::RepoNotFound => OakError::InvalidArgument(
            "not inside a repository: run this from a repo to install the skill \
             project-level, or pass --global to install it user-wide (~/.claude/skills)"
                .to_string(),
        ),
        other => other,
    })?;
    Ok(ctx.work_tree.join(".claude").join("skills"))
}

/// How an installed copy compares with the files baked into this binary.
struct CopyState {
    dir: PathBuf,
    /// Bundled files whose on-disk copy differs.
    stale: Vec<&'static str>,
    /// Bundled files with no on-disk copy.
    missing: Vec<&'static str>,
}

impl CopyState {
    fn inspect(dir: PathBuf) -> CopyState {
        let mut stale = Vec::new();
        let mut missing = Vec::new();
        for (rel, contents) in SKILL_FILES {
            match fs::read_to_string(dir.join(rel)) {
                Ok(cur) if cur == *contents => {}
                Ok(_) => stale.push(*rel),
                Err(_) => missing.push(*rel),
            }
        }
        CopyState {
            dir,
            stale,
            missing,
        }
    }

    /// `current`, `stale` (some files differ or are missing) or `missing`
    /// (nothing installed here).
    fn state(&self) -> &'static str {
        if self.missing.len() == SKILL_FILES.len() {
            "missing"
        } else if self.stale.is_empty() && self.missing.is_empty() {
            "current"
        } else {
            "stale"
        }
    }

    /// Absent lists mean empty, per the JSON contract; a `missing` copy
    /// lists nothing (every file is missing).
    fn to_json(&self, scope: Scope) -> serde_json::Value {
        let mut doc = serde_json::json!({
            "scope": scope.as_str(),
            "path": crate::commands::absolute_path_string(&self.dir),
            "state": self.state(),
        });
        if self.state() == "stale" {
            if !self.stale.is_empty() {
                doc["stale_files"] = serde_json::json!(self.stale);
            }
            if !self.missing.is_empty() {
                doc["missing_files"] = serde_json::json!(self.missing);
            }
        }
        doc
    }
}

/// `oak skill install [--global] [--json]`: write the bundled skill to disk.
///
/// Project-level by default: resolves the enclosing repository the same way
/// every other command does (so it works from any subdirectory) and writes
/// under `<work tree>/.claude/skills/`. With `global`,
/// writes under `~/.claude/skills/` and needs no repository at all.
pub fn install(global: bool, json: bool, cwd: &Path) -> Result<()> {
    let scope = if global {
        Scope::Global
    } else {
        Scope::Project
    };
    let skills_root = match scope {
        Scope::Global => global_skills_root()?,
        Scope::Project => project_skills_root(cwd)?,
    };

    let dir = skills_root.join(SKILL_NAME);
    let mut written: Vec<&str> = Vec::new();
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
        written.push(rel);
    }

    if json {
        return output::print_json(&serde_json::json!({
            "schema_version": 1,
            "kind": "skill_install",
            "skill": SKILL_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "scope": scope.as_str(),
            "path": crate::commands::absolute_path_string(&dir),
            "written_files": written,
            "unchanged_count": unchanged,
            "skill_md": crate::commands::absolute_path_string(&dir.join("SKILL.md")),
            "recommended_next_commands": ["oak skill status --json"],
        }));
    }

    let place = match scope {
        Scope::Global => "all projects (user-wide)".to_string(),
        Scope::Project => format!(
            "this repo ({})",
            skills_root
                .parent()
                .and_then(Path::parent)
                .unwrap_or(&skills_root)
                .display()
        ),
    };
    if written.is_empty() {
        output::success(&format!(
            "Skill {SKILL_NAME} already up to date at {} ({unchanged} files).",
            dir.display()
        ));
    } else {
        output::success(&format!(
            "Installed skill {SKILL_NAME} for {place}: {} file(s) written, \
             {unchanged} already current, at {}.",
            written.len(),
            dir.display()
        ));
    }
    output::info(&format!(
        "Claude Code picks it up automatically. Point other agents at {}, \
         or have them run `oak skill show`.",
        dir.join("SKILL.md").display()
    ));
    Ok(())
}

/// `oak skill status [--json]`: whether the project and user-wide copies
/// match this binary. A copy older than the CLI teaches flags that may have
/// changed, so a stale one names the command that refreshes it.
pub fn status(json: bool, cwd: &Path) -> Result<()> {
    let mut copies: Vec<(Scope, CopyState)> = Vec::new();
    // Outside a repository there is no project copy to report.
    if let Ok(root) = project_skills_root(cwd) {
        copies.push((Scope::Project, CopyState::inspect(root.join(SKILL_NAME))));
    }
    copies.push((
        Scope::Global,
        CopyState::inspect(global_skills_root()?.join(SKILL_NAME)),
    ));

    let next: Vec<&str> = {
        let stale: Vec<&str> = copies
            .iter()
            .filter(|(_, c)| c.state() == "stale")
            .map(|(scope, _)| scope.install_command())
            .collect();
        if !stale.is_empty() {
            stale
        } else if copies.iter().all(|(_, c)| c.state() == "missing") {
            // Nothing installed anywhere: offer the copy that fits here.
            vec![copies[0].0.install_command()]
        } else {
            Vec::new()
        }
    };

    if json {
        return output::print_json(&serde_json::json!({
            "schema_version": 1,
            "kind": "skill_status",
            "skill": SKILL_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "copies": copies.iter().map(|(scope, c)| c.to_json(*scope)).collect::<Vec<_>>(),
            "recommended_next_commands": next,
        }));
    }

    for (scope, copy) in &copies {
        output::print_line(&format!(
            "{:<8} {:<8} {}",
            scope.as_str(),
            copy.state(),
            copy.dir.display()
        ));
    }
    let installed = copies.iter().any(|(_, c)| c.state() != "missing");
    for command in next {
        if installed {
            output::info(&format!("Run `{command}` to bring it up to date."));
        } else {
            output::info(&format!("Run `{command}` to install it."));
        }
    }
    Ok(())
}

/// `oak skill show [FILE]`: print a bundled skill file to stdout, SKILL.md
/// by default, so an agent that doesn't load skills from disk can read the
/// copy that matches this binary without installing anything.
pub fn show(file: Option<&str>) -> Result<()> {
    let wanted = file.unwrap_or("SKILL.md").trim_start_matches("./");
    let found = SKILL_FILES
        .iter()
        .find(|(rel, _)| *rel == wanted || rel.strip_prefix("references/") == Some(wanted));
    match found {
        Some((_, contents)) => {
            output::print_raw(contents);
            Ok(())
        }
        None => Err(OakError::InvalidArgument(format!(
            "no bundled skill file {wanted:?}; the skill ships {}",
            SKILL_FILES
                .iter()
                .map(|(rel, _)| *rel)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// One line for `oak upgrade` to print after replacing the binary: installed
/// copies of the skill still document the old version until reinstalled.
/// The running process still holds the old files, so it can only point at
/// the command, not refresh them itself.
pub fn upgrade_hint() -> Option<String> {
    let mut commands = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(root) = project_skills_root(&cwd) {
            if root.join(SKILL_NAME).join("SKILL.md").exists() {
                commands.push(Scope::Project.install_command());
            }
        }
    }
    if let Ok(root) = global_skills_root() {
        if root.join(SKILL_NAME).join("SKILL.md").exists() {
            commands.push(Scope::Global.install_command());
        }
    }
    if commands.is_empty() {
        return None;
    }
    Some(format!(
        "Refresh the agent skill for the new version: {}",
        commands
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    ))
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
        // Make the temp dir a minimal Oak repo so project-level resolution
        // works. An empty `.oak/` is not a repository; it needs its database.
        fs::create_dir_all(tmp.path().join(".oak")).unwrap();
        oak_core::SqliteRepository::open(&tmp.path().join(".oak/oak.db")).unwrap();

        install(false, false, tmp.path()).unwrap();
        let dir = tmp.path().join(".claude").join("skills").join(SKILL_NAME);
        for (rel, contents) in SKILL_FILES {
            let on_disk = fs::read_to_string(dir.join(rel)).unwrap();
            assert_eq!(&on_disk, contents, "{rel} should match the bundled copy");
        }

        // A stale copy gets refreshed on re-install.
        fs::write(dir.join("SKILL.md"), "stale").unwrap();
        install(false, false, tmp.path()).unwrap();
        let refreshed = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert_ne!(refreshed, "stale", "re-install must refresh stale files");
    }

    #[test]
    fn copy_state_reports_missing_stale_and_current() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(SKILL_NAME);
        assert_eq!(CopyState::inspect(dir.clone()).state(), "missing");

        for (rel, contents) in SKILL_FILES {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        assert_eq!(CopyState::inspect(dir.clone()).state(), "current");

        fs::write(dir.join("SKILL.md"), "older").unwrap();
        fs::remove_file(dir.join("references/merging.md")).unwrap();
        let copy = CopyState::inspect(dir);
        assert_eq!(copy.state(), "stale");
        assert_eq!(copy.stale, ["SKILL.md"]);
        assert_eq!(copy.missing, ["references/merging.md"]);
    }

    #[test]
    fn show_finds_bundled_files_and_names_them_on_a_miss() {
        assert!(show(None).is_ok());
        assert!(show(Some("references/commands.md")).is_ok());
        assert!(show(Some("merging.md")).is_ok());
        let err = show(Some("nope.md")).unwrap_err().to_string();
        assert!(err.contains("references/commands.md"), "{err}");
    }

    /// SKILL.md loads into an agent's context every time the skill fires,
    /// so keep it to the essentials and put detail in `references/`.
    #[test]
    fn skill_md_stays_small() {
        let (_, skill_md) = SKILL_FILES
            .iter()
            .find(|(rel, _)| *rel == "SKILL.md")
            .expect("SKILL.md must be bundled");
        assert!(
            skill_md.len() <= 10_000,
            "SKILL.md is {} bytes; move detail into references/",
            skill_md.len()
        );
    }
}
