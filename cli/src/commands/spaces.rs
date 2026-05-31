//! Agent-space helpers for `oak mount` — scaffold and inspect an "Oak space": a per-repo
//! directory whose children are `oak mount` subdirectories, one per
//! concurrent task on that repo. Designed to be opened in a coding
//! agent (Claude Code, Cursor, etc.) so the agent does branch-per-task
//! work via `oak mount` without polluting the user's machine with full
//! clones.

use std::fs;
use std::path::Path;

use oak_core::{OakError, Result};

use crate::output;

const CLAUDE_MD_TEMPLATE: &str = include_str!("spaces/CLAUDE.md.tmpl");
const SETTINGS_JSON_TEMPLATE: &str = include_str!("spaces/settings.json.tmpl");

/// Render `.claude/settings.json` for a space, substituting the repo spec
/// into the `WorktreeCreate` hook command so Claude Code worktrees mount the
/// right repo.
fn settings_json_content(spec: &str) -> String {
    SETTINGS_JSON_TEMPLATE.replace("{{spec}}", spec)
}

/// Scaffold a space directory for `<owner>/<repo>` at `path`.
///
/// Creates `path` if missing, writes `CLAUDE.md` and
/// `.claude/settings.json`, and substitutes `{{owner}}`, `{{repo}}`,
/// and `{{spec}}` placeholders in the CLAUDE.md template so the agent
/// knows which repo to mount.
pub fn init(owner: &str, repo: &str, path: &Path, force: bool) -> Result<()> {
    if path.exists() && !path.is_dir() {
        return Err(OakError::Server(format!(
            "not a directory: {}",
            path.display()
        )));
    }

    let claude_md = path.join("CLAUDE.md");
    let claude_dir = path.join(".claude");
    let settings_json = claude_dir.join("settings.json");

    if !force {
        if claude_md.exists() {
            return Err(OakError::Server(format!(
                "{} already exists. Pass --force to overwrite.",
                claude_md.display()
            )));
        }
        if settings_json.exists() {
            return Err(OakError::Server(format!(
                "{} already exists. Pass --force to overwrite.",
                settings_json.display()
            )));
        }
    }

    fs::create_dir_all(&claude_dir)?;

    let spec = format!("{owner}/{repo}");
    let claude_md_content = CLAUDE_MD_TEMPLATE
        .replace("{{spec}}", &spec)
        .replace("{{owner}}", owner)
        .replace("{{repo}}", repo);

    fs::write(&claude_md, claude_md_content)?;
    fs::write(&settings_json, settings_json_content(&spec))?;

    output::success(&format!(
        "Scaffolded Oak space for {}{}{} in {}{}{}",
        output::colors::CYAN,
        spec,
        output::colors::RESET,
        output::colors::CYAN,
        path.display(),
        output::colors::RESET,
    ));
    output::item(&format!("wrote {}", claude_md.display()));
    output::item(&format!("wrote {}", settings_json.display()));
    output::blank();
    output::info("Open this directory in Claude Code (or your agent of choice).");
    output::info("Ask it to start a task and it'll create the first mount.");

    Ok(())
}

/// Ensure a space directory exists for `<owner>/<repo>` without overwriting
/// user-edited files. Returns the paths created so callers can summarize what
/// happened before starting the mount.
pub fn ensure(owner: &str, repo: &str, path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if path.exists() && !path.is_dir() {
        return Err(OakError::Server(format!(
            "not a directory: {}",
            path.display()
        )));
    }

    let claude_md = path.join("CLAUDE.md");
    let claude_dir = path.join(".claude");
    let settings_json = claude_dir.join("settings.json");
    fs::create_dir_all(&claude_dir)?;

    let spec = format!("{owner}/{repo}");
    let mut wrote = Vec::new();
    if !claude_md.exists() {
        let claude_md_content = CLAUDE_MD_TEMPLATE
            .replace("{{spec}}", &spec)
            .replace("{{owner}}", owner)
            .replace("{{repo}}", repo);
        fs::write(&claude_md, claude_md_content)?;
        wrote.push(claude_md);
    }
    if !settings_json.exists() {
        fs::write(&settings_json, settings_json_content(&spec))?;
        wrote.push(settings_json);
    }
    Ok(wrote)
}

/// Summarize active mounts under `cwd`, flagging any with uncommitted
/// overlay. Designed to be called by a Claude Code Stop hook.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn status(cwd: &Path, quiet: bool) -> Result<()> {
    use crate::commands::mount::state::{
        canonical_key, load_config, load_index, load_overlay_meta, state_dir_for,
    };

    let idx = load_index()?;
    let cwd_key = canonical_key(cwd);

    let mut dirty: Vec<DirtyMountSummary> = Vec::new();
    let mut clean_count = 0usize;

    for (mount_point, id) in &idx.mounts {
        if !mount_point.starts_with(&cwd_key) {
            continue;
        }

        let Ok(state_dir) = state_dir_for(id) else {
            continue;
        };
        let Ok(cfg) = load_config(&state_dir) else {
            continue;
        };
        let overlay = load_overlay_meta(&state_dir).unwrap_or_default();

        let modified = overlay.dirty.len();
        let deleted = overlay.deletions.len();
        let renamed = overlay.renames.len();

        if modified + deleted + renamed > 0 {
            dirty.push(DirtyMountSummary {
                mount_point: mount_point.clone(),
                virtual_branch: cfg.virtual_branch.clone(),
                modified,
                deleted,
                renamed,
            });
        } else {
            clean_count += 1;
        }
    }

    if dirty.is_empty() {
        if !quiet {
            if clean_count == 0 {
                output::info("No active mounts under this directory.");
            } else {
                output::success(&format!(
                    "All {clean_count} active mount(s) under this directory are committed."
                ));
            }
        }
        return Ok(());
    }

    output::warning(&format!(
        "{} mount(s) have uncommitted overlay:",
        dirty.len()
    ));
    for d in &dirty {
        let mut parts: Vec<String> = Vec::with_capacity(3);
        if d.modified > 0 {
            parts.push(format!("{} modified", d.modified));
        }
        if d.deleted > 0 {
            parts.push(format!("{} deleted", d.deleted));
        }
        if d.renamed > 0 {
            parts.push(format!("{} renamed", d.renamed));
        }
        output::item(&format!(
            "{} (branch {}): {}",
            d.mount_point,
            d.virtual_branch,
            parts.join(", "),
        ));
    }
    output::info("Run `oak commit` inside each mount, then `oak push` before ending.");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn status(_cwd: &Path, quiet: bool) -> Result<()> {
    if !quiet {
        output::info("oak mount is not supported on this platform.");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
struct DirtyMountSummary {
    mount_point: String,
    virtual_branch: String,
    modified: usize,
    deleted: usize,
    renamed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_json_substitutes_spec_into_worktree_hook() {
        let rendered = settings_json_content("oak/myrepo");
        assert!(
            !rendered.contains("{{spec}}"),
            "spec placeholder unresolved"
        );
        assert!(rendered.contains("oak mount worktree-create oak/myrepo"));
        assert!(rendered.contains("oak mount worktree-remove"));
        // Must still be valid JSON with both hook events registered.
        let v: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(v["hooks"]["WorktreeCreate"].is_array());
        assert!(v["hooks"]["WorktreeRemove"].is_array());
        assert!(v["hooks"]["Stop"].is_array());
    }

    #[test]
    fn ensure_writes_settings_with_resolved_spec() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wrote = ensure("oak", "myrepo", tmp.path()).unwrap();
        assert_eq!(wrote.len(), 2, "expected CLAUDE.md + settings.json");
        let settings =
            fs::read_to_string(tmp.path().join(".claude").join("settings.json")).unwrap();
        assert!(settings.contains("oak mount worktree-create oak/myrepo"));
        assert!(!settings.contains("{{spec}}"));
    }
}
