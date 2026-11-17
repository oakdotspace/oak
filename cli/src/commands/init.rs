use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use dialoguer::Confirm;
use oak_core::{Branch, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Whether `oak_dir` already holds a repository that must not be overwritten.
///
/// A `.oak/` without its database is an incomplete repository, or just tracked
/// content such as `.oak/workflows` or `.oak/attributes`,
/// so it may be completed in place. A database or an interrupted clone means
/// there is already something to keep, and a `.oak` that isn't a directory
/// can't hold one.
pub(crate) fn oak_dir_holds_repository(oak_dir: &Path) -> bool {
    oak_dir.join("oak.db").exists()
        || oak_dir.join("CLONE_IN_PROGRESS").exists()
        || (oak_dir.exists() && !oak_dir.is_dir())
}

/// Initialize a new local-only Oak repository.
///
/// `interactive` controls the optional setup prompts (git-history import,
/// `.oakignore`, `AGENTS.md`, `.git` cleanup). The caller decides — rather
/// than this function probing the TTY itself — so in-process callers like
/// tests can force a non-interactive init even when stdin happens to be a
/// terminal. The real CLI passes `std::io::stdin().is_terminal()`.
/// What `oak init` created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    /// The branch the new repository starts on.
    pub branch: String,
    /// Commits imported from an existing git repository (0 when none).
    pub imported_commits: usize,
    /// What the git import brought across and left out, when one ran.
    pub import: Option<crate::commands::git_clone::ImportReport>,
}

/// Undo a repository `oak init` just created: its database files, and the
/// `.oak/` directory itself when init made it and nothing else is in it.
fn remove_new_repository(oak_dir: &Path, oak_dir_existed: bool) {
    for name in ["oak.db", "oak.db-wal", "oak.db-shm", "oak.db-journal"] {
        let _ = fs::remove_file(oak_dir.join(name));
    }
    if !oak_dir_existed {
        let _ = fs::remove_dir(oak_dir);
    }
}

/// What `oak init` does with an existing `.git` directory in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHistory {
    /// Ask on a terminal. Without one, refuse rather than guess: starting
    /// empty beside a git history would make the first `oak commit` one
    /// commit holding every file, with the history left behind.
    Ask,
    /// Import the git history (`--import-git`).
    Import,
    /// Start empty and leave `.git` alone (`--no-import-git`).
    Skip,
}

pub fn run(path: &Path, interactive: bool) -> Result<()> {
    run_reporting(path, interactive, GitHistory::Ask).map(|_| ())
}

/// [`run`], returning what was created (for the `--json` receipt).
pub fn run_reporting(path: &Path, interactive: bool, git: GitHistory) -> Result<InitOutcome> {
    let oak_dir = path.join(".oak");

    // `<home>/.oak` is Oak's global state directory (credentials),
    // never a repository — the same predicate discovery uses, so init never
    // creates a repository that `oak status` would then refuse to open.
    match crate::resolve::global_state_reason(path) {
        Some(None) => {
            return Err(OakError::InvalidArgument(format!(
                "refusing to initialize a repository in your home directory ({}): its .oak/ is Oak's global state directory. Run `oak init` in a project directory instead.",
                path.display()
            )));
        }
        Some(Some(marker)) => {
            return Err(OakError::InvalidArgument(format!(
                "refusing to initialize a repository in {}: its .oak/ contains `{marker}`, which only Oak's per-user global state directory (~/.oak) holds. If this is not a global state directory, remove {} and re-run `oak init`; otherwise run `oak init` in a project directory instead.",
                path.display(),
                path.join(".oak").join(marker).display()
            )));
        }
        None => {}
    }
    if oak_dir_holds_repository(&oak_dir) {
        return Err(OakError::RepoAlreadyExists);
    }

    // If we're inside an existing git working tree, offer to import its
    // history rather than starting from an empty oak repo. Same pipeline that
    // `oak clone <git-url>` uses — we just point it at the local work tree
    // instead of cloning first.
    let git_dir = path.join(".git");
    let import_git = if !git_dir.exists() {
        if git == GitHistory::Import {
            return Err(OakError::InvalidArgument(format!(
                "--import-git: {} has no .git to import",
                path.display()
            )));
        }
        false
    } else {
        match git {
            GitHistory::Import => true,
            GitHistory::Skip => false,
            GitHistory::Ask if interactive => Confirm::new()
                .with_prompt(
                    "Detected an existing git repository here. Import its history into oak?",
                )
                .default(true)
                .interact()
                .map_err(|e| OakError::Io(std::io::Error::other(e)))?,
            GitHistory::Ask => {
                return Err(OakError::InvalidArgument(format!(
                    "{} is a git repository. Run `oak init --import-git` to bring its history into Oak, or `oak init --no-import-git` to start an empty repository beside it.",
                    path.display()
                )));
            }
        }
    };
    // Read the git history before creating anything, so a repository Oak
    // can't open (or a corrupt one) leaves no half-made `.oak/` behind.
    if import_git {
        crate::commands::git_clone::check_importable(path)?;
    }

    // Create .oak directory
    let oak_dir_existed = oak_dir.exists();
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

    let report = if import_git {
        // Create the local `main` row that `convert_history` writes into, then
        // replay every git commit onto it, and point the personal branch at
        // the same tip so work starts where git left off.
        //
        // The working tree is left exactly as it is: this is the user's own
        // checkout, and rewriting it from the imported tip would discard
        // uncommitted git changes and delete untracked files. Anything that
        // differs from the last git commit shows up in `oak status` instead.
        let imported = (|| -> Result<crate::commands::git_clone::ImportReport> {
            let main_branch = Branch::new("main".to_string(), None, None);
            repo.store_branch(&main_branch)?;
            let report = crate::commands::git_clone::convert_history(path, &repo)?;
            if let Some(tip) = repo.get_branch_head("main")? {
                repo.set_branch_head(&branch_name, &tip)?;
                repo.set_head(&tip)?;
            }
            Ok(report)
        })();
        match imported {
            Ok(report) => Some(report),
            Err(e) => {
                // Leave no half-imported repository behind: a retry of
                // `oak init` would otherwise refuse it as already existing.
                drop(repo);
                remove_new_repository(&oak_dir, oak_dir_existed);
                return Err(e);
            }
        }
    } else {
        None
    };
    let imported = report.as_ref().map_or(0, |r| r.commits);

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
    if let Some(report) = &report {
        report.warn_about_skips();
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

    Ok(InitOutcome {
        branch: branch_name,
        imported_commits: imported,
        import: report,
    })
}

fn oak_agents_md_contents(branch_name: &str) -> String {
    format!(
        r#"# Oak repository

This project uses [Oak](https://oak.space) for version control, not Git.
Do not run `git` commands here. Use `oak`.

For the full guide, install the agent skill (`oak skill install`) or read it
with `oak skill show`.

## Every task

```bash
oak start "<one-line task summary>"   # new branch from latest main, described and pushed
# ...work, `oak commit` as you go...
oak pull                              # merge in the latest main; on a CONFLICT: `oak conflict status`, fix, `oak pull --continue`
oak diff --branch --stat              # confirm the branch changes only your files
oak finish --desc-file <file> --json  # describe, checkpoint, publish
```

## How it differs from Git

- No staging area: `oak commit` checkpoints every change (or the paths given).
- Commits have no message (`-m` is refused). The branch description
  (`oak desc "..."`) is the narrative and becomes the squash-merge message.
- Commits stay local until `oak push`, `oak commit --push` or `oak finish`.
- No stash: `oak switch -c NAME` carries uncommitted files to the new branch,
  and `oak switch -c --clean` starts from fresh main without them.
- Prefer `--json`: every payload lists `recommended_next_commands`.
- Use the non-interactive forms: `oak diff --print` (bare `oak diff` opens a
  browser UI), and always give `oak switch` a branch name.

## Key commands

```bash
oak status --json --compact # what changed
oak diff --print            # changes vs HEAD as text
oak log -n 10               # history
oak switch my-feature       # switch to an existing branch
oak agent state --json      # where am I, and what next
```

Don't land your own work unless asked:
```bash
oak merge                   # squash-merge this branch into main, on the server
```

You are on branch **{branch_name}** (parented onto `main`). Run
`oak --help` for every command.
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
/// Author derivation: `OAK_AUTHOR`, then the locally stored `oak whoami`
/// username for the default remote, then `USER`, then `USERNAME`, falling back
/// to `"work"`. Whitespace, slashes, and uppercase are sanitized.
pub fn default_local_branch_name() -> String {
    format!("{}-{}", author_slug(), random_branch_suffix())
}

/// Sanitize the author env var into a branch-safe slug. Public so callers
/// that want the un-suffixed author component (display, search) can reuse
/// the exact same derivation.
pub fn author_slug() -> String {
    let raw = super::credentials::preferred_author_name("work");

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
