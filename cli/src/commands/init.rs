use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use oak_core::{detect_engine, Branch, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Initialize a new local-only Oak repository.
///
/// `interactive` controls the optional setup prompts (git-history import,
/// `.oakignore`, `AGENTS.md`, `.git` cleanup). The caller decides — rather
/// than this function probing the TTY itself — so in-process callers like
/// tests can force a non-interactive init even when stdin happens to be a
/// terminal. The real CLI passes `std::io::stdin().is_terminal()`.
pub fn run(path: &Path, interactive: bool) -> Result<()> {
    let oak_dir = path.join(".oak");

    if oak_dir.exists() {
        return Err(OakError::RepoAlreadyExists);
    }

    // If we're inside an existing git working tree, offer to import its
    // history rather than starting from an empty oak repo. Same pipeline that
    // `oak clone <git-url>` uses — we just point it at the local work tree
    // instead of cloning first.
    let git_dir = path.join(".git");
    let import_git = git_dir.exists()
        && interactive
        && Confirm::new()
            .with_prompt("Detected an existing git repository here. Import its history into oak?")
            .default(true)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    // Create .oak directory
    fs::create_dir_all(&oak_dir)?;

    // Initialize SQLite database
    let db_path = oak_dir.join("oak.db");
    let repo = SqliteRepository::open(&db_path)?;

    // Set repo name from directory name
    let repo_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());
    repo.set_metadata(MetadataKey::RepoName, &repo_name)?;

    // `main` exists only on the server. Locally the user always works on a
    // branch (parented onto main conceptually). Default to a personal branch
    // named after the author. Falls back to "work" when we can't get a name.
    let branch_name = default_local_branch_name();
    let default_branch = Branch::new(branch_name.clone(), None, Some("main".to_string()));
    repo.store_branch(&default_branch)?;
    repo.set_current_branch(&branch_name)?;

    let imported = if import_git {
        // Create the local `main` row that `convert_history` writes into, then
        // replay every git commit onto it. Afterwards fast-forward the user's
        // personal branch to the same tip so they start where git left off.
        let main_branch = Branch::new("main".to_string(), None, None);
        repo.store_branch(&main_branch)?;

        let converted = crate::commands::git_clone::convert_history(path, &repo)?;
        if converted > 0 {
            if let Some(tip) = repo.get_branch_head("main")? {
                repo.set_branch_head(&branch_name, &tip)?;
                repo.set_head(&tip)?;

                // Materialize the working tree from oak's manifest so the very
                // first `oak status` is clean. git left the tree as it checked
                // it out (`.gitattributes` smudge filters applied, submodule
                // dirs populated), but the manifest is built from git's raw
                // blobs with gitlinks dropped — see the matching note in
                // `git_clone::run`.
                if let Some(commit) = repo.get_commit(&tip)? {
                    if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
                        let lock = crate::workdir_lock::WorkdirLock::acquire(&oak_dir)?;
                        crate::commands::switch::update_working_dir(&lock, path, &repo, &manifest)?;
                    }
                }
            }
        }
        converted
    } else {
        0
    };

    let quiet_stdout = !std::io::stdout().is_terminal();

    if !quiet_stdout {
        if imported > 0 {
            output::success(&format!(
                "Initialized repository in {}{}{} with {} commit(s) imported from git",
                output::colors::CYAN,
                oak_dir.display(),
                output::colors::RESET,
                imported,
            ));
        } else {
            if import_git {
                output::info("Git repository has no commits; oak repo initialized empty.");
            }
            output::success(&format!(
                "Initialized empty repository in {}{}{}",
                output::colors::CYAN,
                oak_dir.display(),
                output::colors::RESET,
            ));
        }
        output::item(&format!(
            "Working on branch {}{}{} (parented onto main)",
            output::colors::CYAN,
            branch_name,
            output::colors::RESET,
        ));
    }

    // Detect game engine and offer to create .oakignore
    if let Some(engine) = detect_engine(path) {
        let oakignore_path = path.join(".oakignore");
        if !oakignore_path.exists() && interactive {
            let create = Confirm::new()
                .with_prompt(format!(
                    "Detected {engine} project. Create .oakignore with recommended ignore patterns? (highly recommended)"
                ))
                .default(true)
                .interact()
                .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

            if create {
                fs::write(&oakignore_path, engine.oakignore_contents())?;
                if !quiet_stdout {
                    output::success("Created .oakignore with recommended ignore patterns");
                }
            }
        }
    }

    // Offer to write an AGENTS.md so coding agents know this is an Oak repo.
    // AGENTS.md holds the content (Codex, Cursor, … read it directly); a
    // one-line CLAUDE.md `@AGENTS.md` import makes Claude Code — which loads
    // CLAUDE.md, not AGENTS.md — pick up the same instructions without
    // duplicating them.
    let agents_md_path = path.join("AGENTS.md");
    if !agents_md_path.exists() && interactive {
        let write = Confirm::new()
            .with_prompt("Write AGENTS.md so AI coding agents know this is an Oak repository?")
            .default(true)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

        if write {
            fs::write(&agents_md_path, oak_agents_md_contents(&branch_name))?;
            let claude_md_path = path.join("CLAUDE.md");
            if !claude_md_path.exists() {
                fs::write(&claude_md_path, "@AGENTS.md\n")?;
            }
            if !quiet_stdout {
                output::success(
                    "Created AGENTS.md (+ CLAUDE.md import) with Oak context for AI coding agents",
                );
            }
        }
    }

    // Offer to remove the .git directory now that the history lives in oak.
    // Only when we actually imported — if the user declined the import we
    // leave .git alone because they likely still use it.
    if imported > 0 && git_dir.exists() && interactive {
        if !quiet_stdout {
            output::blank();
        }
        let prompt = format!(
            "Conversion complete. Would you like to clean up the temporary '{}' directory?",
            git_dir.display()
        );
        let remove = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
        if remove {
            fs::remove_dir_all(&git_dir)?;
            if !quiet_stdout {
                output::success(&format!("Removed {}", git_dir.display()));
            }
        } else if !quiet_stdout {
            output::info(&format!(
                "Left '{}' in place. You can delete it later with `rm -rf {}`.",
                git_dir.display(),
                git_dir.display()
            ));
        }
    }

    Ok(())
}

fn oak_agents_md_contents(branch_name: &str) -> String {
    format!(
        r#"# Oak Repository

This project uses [Oak](https://oak.space) for version control — **not Git**.
Do not run `git` commands. Use `oak` instead.

## Key commands

```bash
oak status          # show changed files
oak diff            # show changes vs HEAD
oak commit          # snapshot the working directory (no message needed)
oak log             # show commit history
oak push            # push commits to the remote server
oak pull            # pull latest commits from the remote server
```

## Branching

```bash
oak switch -c               # create a generated branch from latest available main (keeps dirty files)
oak switch -c my-feature    # create a named branch from latest available main (keeps dirty files)
oak switch -c --clean       # create a clean generated branch, discarding dirty files
oak desc "what this branch does"   # set the current branch's description
oak switch                  # pick a branch interactively
oak switch my-feature       # switch to an existing branch (fetched from the remote when not local)
oak merge                   # merge current branch into its parent
```

You are currently on branch **{branch_name}** (parented onto `main`).
Commit freely — your changes are isolated until you `oak merge` or open a PR
on oak.space.

## What Oak is

Oak is a version control system designed for AI-assisted workflows.
Every session gets its own branch. Commits have no messages; the branch
description is the narrative. Large binary files are handled natively via
content-defined chunking — no LFS required.

See `oak --help` for the full command reference.
"#,
        branch_name = branch_name,
    )
}

/// Pick a default name for the user's local branch on this clone.
///
/// Format: `<author>-<rand6hex>` (e.g. `zdgeier-3f2a8b`). The random suffix
/// makes each clone of the same repo get its own server-side branch — two
/// clones by the same user no longer collide on push. Each new branch is
/// cheap (branch-per-session is the expected workflow), so we don't bother
/// trying to recycle prior names.
///
/// Author derivation: `OAK_AUTHOR`, then `USER`, then `USERNAME`, falling
/// back to `"work"`. Whitespace, slashes, and uppercase are sanitized.
pub fn default_local_branch_name() -> String {
    format!("{}-{}", author_slug(), random_branch_suffix())
}

/// Sanitize the author env var into a branch-safe slug. Public so callers
/// that want the un-suffixed author component (display, search) can reuse
/// the exact same derivation.
pub fn author_slug() -> String {
    let raw = std::env::var("OAK_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "work".to_string());

    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();

    if cleaned.is_empty() || cleaned == "main" {
        "work".to_string()
    } else {
        cleaned
    }
}

/// 6 hex chars from a fresh uuid v4. ~16M combinations is plenty for a
/// per-user, per-server uniqueness check — collisions are vanishingly rare,
/// and a stray collision just surfaces as a regular push conflict the user
/// can resolve by renaming.
pub fn random_branch_suffix() -> String {
    let id = uuid::Uuid::new_v4();
    let bytes = id.as_bytes();
    format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

#[cfg(test)]
mod tests {
    use super::oak_agents_md_contents;

    #[test]
    fn agents_md_uses_surfaced_clean_flag_not_hidden_discard_alias() {
        let contents = oak_agents_md_contents("agent-branch");

        assert!(contents.contains("oak switch -c --clean"));
        assert!(!contents.contains("oak switch -c --discard"));
        assert_eq!(contents.matches("\noak merge ").count(), 1);
    }
}
