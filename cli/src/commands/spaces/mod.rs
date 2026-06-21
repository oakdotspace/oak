//! `oak space` — scaffold and manage an "Oak space".
//!
//! An Oak space is a directory where an agent works across one **org's** repos,
//! mounting whichever repos a task needs. Each task gets its own subdirectory
//! under the space, holding one `oak mount` per repo it touches — a lazy,
//! on-demand view on its own virtual branch. It plays the role git worktrees do
//! for agent workflows, but without a full clone behind each one (mounts
//! hydrate content lazily, so a fresh task is near-instant) and across a whole
//! org rather than a single repo, so cross-repo changes live in one space.
//!
//! `oak space new <org> [path]` lays down the space directory with the
//! agent-facing `AGENTS.md`, a `CLAUDE.md` pointer, a `.claude/settings.json`,
//! and a `.oak-space` marker recording the org. The agent then lists the org's
//! repos (`oak space repos`) and mounts the ones a task needs.
//!
//! `oak space repos [org]` lists the repos in the space's org (read from the
//! `.oak-space` marker, or passed explicitly) so an agent can pick the right
//! one(s) for a task.
//!
//! `oak space clean [path]` tears down every mount under the space whose
//! working tree is clean — the finished/merged tasks — leaving dirty mounts
//! alone unless `--force` is given.

use std::fs;
use std::path::{Path, PathBuf};

use oak_core::{OakError, Result};

use crate::commands::mount::{remote, token_for};
use crate::output;

/// Per-space agent instructions. `{{org}}` is substituted with the org slug.
const AGENTS_MD_TMPL: &str = include_str!("AGENTS.md.tmpl");

/// `.claude/settings.json` for the space. Also uses `{{org}}`.
const SETTINGS_JSON_TMPL: &str = include_str!("settings.json.tmpl");

/// `CLAUDE.md` is a one-line `@AGENTS.md` import. Claude Code auto-loads
/// `CLAUDE.md` (not `AGENTS.md`) and expands `@`-path imports, so this points
/// it at the real instructions without duplicating them. Codex / Cursor read
/// `AGENTS.md` directly.
const CLAUDE_MD_POINTER: &str = "@AGENTS.md\n";

/// Filename of the marker, written in the space root, that records the org a
/// space is bound to. `oak space repos` (and future space-aware commands) read
/// it so they don't have to be told the org every time.
const SPACE_MARKER: &str = ".oak-space";

/// Resolve a space's org slug from a user-supplied spec. Accepts a bare org
/// (`oak`) or a legacy `org/repo` spec (`oak/oak`), in which case only the org
/// segment is used — a space now spans the whole org, not a single repo.
pub fn parse_org(spec: &str) -> Result<String> {
    let org = spec.trim().split('/').next().unwrap_or("").trim();
    if org.is_empty() || org == "." || org == ".." {
        return Err(OakError::InvalidArgument(format!(
            "Expected an org slug (e.g. `oak`), got '{spec}'"
        )));
    }
    if org
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(OakError::InvalidArgument(format!(
            "Invalid org slug '{org}': org cannot contain whitespace, control characters, or backslashes"
        )));
    }
    Ok(org.to_string())
}

/// Resolve a possibly-relative path against `cwd`.
fn abs(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Walk up from `start` looking for a `.oak-space` marker and return the org
/// slug it records. Lets `oak space repos` (and friends) work with no argument
/// when run anywhere inside a space.
fn find_space_org(start: &Path) -> Option<String> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if let Ok(contents) = fs::read_to_string(d.join(SPACE_MARKER)) {
            let org = contents.trim();
            if !org.is_empty() {
                return Some(org.to_string());
            }
        }
        dir = d.parent();
    }
    None
}

/// Fetch the names of the repos in `org` that the caller can see. Reuses the
/// same `/api/repos` listing that `oak mount <org>` uses, filtered to the org.
async fn fetch_org_repos(remote_url: &str, org: &str) -> Result<Vec<String>> {
    let token = token_for(remote_url);
    let mut repos = remote::list_repos(remote_url, token.as_deref()).await?;
    repos.retain(|(o, _)| o == org);
    Ok(repos.into_iter().map(|(_, repo)| repo).collect())
}

/// `oak space new <org> [dest]` — scaffold an Oak space for an org.
///
/// Creates the space directory (default `./<org>`), writes the agent
/// instructions, the Claude Code settings, and a `.oak-space` marker recording
/// the org, then leaves the directory ready for per-task `oak mount`s of any of
/// the org's repos. Refuses to write into a non-empty directory so it never
/// clobbers existing work, and never overwrites a file that's already there.
///
/// As a convenience it also tries to list the org's repos so the user sees
/// what's available; a listing failure (offline, no auth) is non-fatal — the
/// space is still scaffolded.
pub async fn new(spec: &str, dest: Option<&Path>, cwd: &Path, remote_url: &str) -> Result<()> {
    let org = parse_org(spec)?;

    let dir = match dest {
        Some(d) => abs(d, cwd),
        None => cwd.join(&org),
    };

    // Don't scaffold over an existing non-empty directory — that's almost
    // always a mistake (a checkout, another space, unrelated files).
    if dir.exists() {
        let non_empty = fs::read_dir(&dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(OakError::InvalidArgument(format!(
                "{} already exists and isn't empty; pick a different path for the space.",
                dir.display()
            )));
        }
    }

    fs::create_dir_all(&dir)?;
    fs::create_dir_all(dir.join(".claude"))?;

    let agents = AGENTS_MD_TMPL.replace("{{org}}", &org);
    let settings = SETTINGS_JSON_TMPL.replace("{{org}}", &org);

    write_if_absent(&dir.join("AGENTS.md"), &agents)?;
    write_if_absent(&dir.join("CLAUDE.md"), CLAUDE_MD_POINTER)?;
    write_if_absent(&dir.join(".claude").join("settings.json"), &settings)?;
    write_if_absent(&dir.join(SPACE_MARKER), &format!("{org}\n"))?;

    output::success(&format!(
        "Created Oak space for org {org} at {}",
        dir.display()
    ));

    // Show what the agent will have to choose from. Best-effort only.
    match fetch_org_repos(remote_url, &org).await {
        Ok(repos) if !repos.is_empty() => {
            output::info(&format!("{} repo(s) in {org}:", repos.len()));
            for repo in &repos {
                println!("  {org}/{repo}");
            }
        }
        Ok(_) => output::info(&format!(
            "No repos visible in {org} yet (empty org, or you may need to log in)."
        )),
        Err(e) => output::warning(&format!(
            "Couldn't list repos for {org}: {e} (space scaffolded anyway)."
        )),
    }

    output::info("Next: open a coding agent here. It will list the org's repos with:");
    println!("  oak space repos");
    output::info("and mount the one(s) a task needs — see AGENTS.md in the new space.");
    Ok(())
}

/// `oak space repos [org]` — list the repos in the space's org.
///
/// With no `org`, reads the `.oak-space` marker from the current directory (or
/// any ancestor), so it Just Works when run inside a space. The agent uses this
/// to pick which repo(s) a task should mount.
pub async fn repos(org: Option<&str>, cwd: &Path, remote_url: &str) -> Result<()> {
    let org = match org {
        Some(o) => parse_org(o)?,
        None => find_space_org(cwd).ok_or_else(|| {
            OakError::InvalidArgument(
                "no org given and no .oak-space marker found here; run this inside an Oak space, \
                 or pass the org explicitly (`oak space repos <org>`)."
                    .to_string(),
            )
        })?,
    };

    let repos = fetch_org_repos(remote_url, &org).await?;
    if repos.is_empty() {
        output::info(&format!("No repos visible in {org}."));
        return Ok(());
    }

    output::info(&format!("{} repo(s) in {org}:", repos.len()));
    for repo in &repos {
        println!("  {org}/{repo}");
    }
    output::info("Mount one into a task with:");
    println!("  oak mount {org}/<repo> ./<task-slug>/<repo>");
    Ok(())
}

/// Write `contents` to `path` only if nothing is there yet. Scaffolding should
/// be additive: re-running `oak space new` on a half-set-up dir fills the gaps
/// without stomping edits the user (or a previous run) made.
fn write_if_absent(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        output::info(&format!(
            "{} already exists, leaving it as-is.",
            path.display()
        ));
        return Ok(());
    }
    fs::write(path, contents)?;
    Ok(())
}

/// `oak space clean [dest]` — tear down finished mounts under a space.
///
/// "Finished" = the mount's overlay is clean (no uncommitted changes) *and*
/// every commit on its virtual branch has been pushed, which is the state a
/// task is in after `oak commit` + `oak push` (and merge). Mounts holding
/// in-flight work — dirty or committed-but-unpushed — are reported and
/// skipped so it's never lost, unless `force` is set, in which case they're
/// torn down too.
pub fn clean(dest: Option<&Path>, cwd: &Path, force: bool) -> Result<()> {
    use crate::commands::mount;

    let root = match dest {
        Some(d) => abs(d, cwd),
        None => cwd.to_path_buf(),
    };
    // The mount registry stores canonical absolute paths; canonicalize the
    // space root the same way so the prefix test matches.
    let root_key = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());

    let mut targets: Vec<PathBuf> = mount::state::load_index()?
        .mounts
        .keys()
        .map(PathBuf::from)
        .filter(|p| p.starts_with(&root_key))
        .collect();
    targets.sort();

    if targets.is_empty() {
        output::info(&format!("No mounts under {}.", root.display()));
        return Ok(());
    }

    let mut cleaned = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for dest in &targets {
        if !force {
            if let Some(why) = mount_in_flight(dest) {
                output::warning(&format!(
                    "{}: {why} — skipping (commit + push, or use --force).",
                    dest.display()
                ));
                skipped += 1;
                continue;
            }
        }
        match mount::end(dest, force) {
            Ok(()) => cleaned += 1,
            Err(e) => {
                failed += 1;
                output::warning(&format!("{}: {e}", dest.display()));
            }
        }
    }

    output::success(&format!(
        "Cleaned {cleaned} mount(s); {skipped} skipped (in-flight work), {failed} failed."
    ));
    Ok(())
}

/// Best-effort in-flight-work check for a mount: any uncommitted overlay
/// content, or commits on the virtual branch that were never pushed (those
/// live solely in the mount's state dir, so tearing it down would destroy
/// them). Returns a human-readable reason, or `None` when the mount is safe
/// to clean. Missing/unreadable state reads as safe so cleanup can still
/// proceed (and `mount::end` will surface a real error if something's wrong).
fn mount_in_flight(dest: &Path) -> Option<String> {
    use crate::commands::mount;
    use crate::commands::mount::state::{load_overlay_meta, lookup_id_for, state_dir_for};

    let Ok(Some(id)) = lookup_id_for(dest) else {
        return None;
    };
    let Ok(state_dir) = state_dir_for(&id) else {
        return None;
    };
    let overlay = load_overlay_meta(&state_dir).unwrap_or_default();
    if !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty() {
        return Some("uncommitted changes".to_string());
    }
    // An unreadable overlay or cache must count as in-flight: `space clean`
    // skips in-flight mounts, and we'd rather skip than risk discarding work.
    if mount::state::overlay_state_unreadable(&state_dir) {
        return Some("unreadable overlay state".to_string());
    }
    match mount::unpushed_commits_checked(&state_dir) {
        Some(0) => None,
        Some(n) => Some(format!("{n} unpushed commit(s)")),
        None => Some("unverifiable mount state".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_org_accepts_bare_and_legacy_repo_specs() {
        assert_eq!(parse_org("oak").unwrap(), "oak");
        assert_eq!(parse_org("oak/oak").unwrap(), "oak");
    }

    #[test]
    fn parse_org_rejects_reserved_and_unsafe_slugs() {
        for spec in [
            "",
            ".",
            "..",
            "../repo",
            "./repo",
            "team docs",
            "team\\docs",
            "team\tdocs",
            "team\ndocs",
            "team\u{1b}docs",
            "team\u{7f}docs",
            "team\0docs",
        ] {
            let err = parse_org(spec).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "{spec:?} should be InvalidArgument, got {err:?}"
            );
        }
    }
}
