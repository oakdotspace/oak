//! Remote operations for git-backed repositories.
//!
//! A git-backed Oak repo (one resolved from `.git/`, see
//! [`crate::resolve::Backend::Git`]) stores everything as ordinary git objects
//! and refs, so it can be hosted anywhere git can: GitHub, GitLab, a bare repo
//! on a server. The Oak server protocol (chunked push, server-side merge, CI)
//! does not apply there. Instead, like jj, these commands delegate transport to
//! the system `git` binary, which already knows the user's SSH keys and
//! credential helpers (`gh auth setup-git`, the macOS keychain, ...).
//!
//! Supported: `oak push`, `oak fetch`, `oak pull`, `oak commit --push`,
//! `oak finish`, and `oak clone <git-url> --git`. `oak merge` points at the
//! host's review flow (a GitHub pull request) instead of merging server-side.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use oak_core::{OakError, Repository, Result};
use serde::Serialize;

use crate::output;
use crate::resolve::{Backend, RepoContext};

const DEFAULT_REMOTE: &str = "origin";

/// The resolved context when `path` is inside a git-backed repo, else `None`.
/// Errors resolving the repo are swallowed here so the caller's normal path
/// reports them in its own words.
pub fn git_backend(path: &Path) -> Option<RepoContext> {
    match crate::resolve::resolve(path) {
        Ok(ctx) if matches!(ctx.backend, Backend::Git { .. }) => Some(ctx),
        _ => None,
    }
}

/// Run `git -C <work_tree> <args>` and capture its output.
fn git_output(work_tree: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            OakError::Git(format!(
                "failed to run `git` (is git installed and on PATH?): {e}"
            ))
        })
}

/// Run git and return trimmed stdout, or an error carrying git's stderr.
fn git_stdout(work_tree: &Path, args: &[&str]) -> Result<String> {
    let out = git_output(work_tree, args)?;
    if !out.status.success() {
        return Err(OakError::Git(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run git with its progress/stderr streamed to the user's terminal (stderr),
/// keeping stdout clean for `--json` callers.
fn git_streamed(work_tree: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(work_tree)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            OakError::Git(format!(
                "failed to run `git` (is git installed and on PATH?): {e}"
            ))
        })?;
    Ok(status.success())
}

fn current_branch(repo: &dyn Repository) -> Result<String> {
    repo.get_current_branch_name()?.ok_or_else(|| {
        OakError::InvalidArgument(
            "HEAD is detached; switch to a branch (`oak switch -c <name>`) first".to_string(),
        )
    })
}

/// Pick the remote: an explicit name or URL, else the branch's configured
/// upstream remote, else `origin`. Errors when nothing is configured so the
/// user learns how to link a GitHub repo instead of seeing git's message.
fn resolve_remote(ctx: &RepoContext, explicit: Option<&str>, branch: &str) -> Result<String> {
    if let Some(remote) = explicit {
        return Ok(remote.to_string());
    }
    let configured = git_stdout(
        &ctx.work_tree,
        &["config", &format!("branch.{branch}.remote")],
    )
    .ok()
    .filter(|r| !r.is_empty());
    let remote = configured.unwrap_or_else(|| DEFAULT_REMOTE.to_string());
    let remotes = git_stdout(&ctx.work_tree, &["remote"])?;
    if !remotes.lines().any(|r| r == remote) {
        return Err(OakError::Config(format!(
            "No git remote named '{remote}'. Link a host first, e.g.\n  \
             git remote add origin git@github.com:<owner>/<repo>.git\n\
             or create one with `gh repo create --source . --remote origin`."
        )));
    }
    Ok(remote)
}

/// The URL configured for a named remote (or the remote itself if it's a URL).
fn remote_url(ctx: &RepoContext, remote: &str) -> Option<String> {
    git_stdout(&ctx.work_tree, &["remote", "get-url", remote])
        .ok()
        .or_else(|| remote.contains(['/', ':']).then(|| remote.to_string()))
}

/// The remote's default branch (`refs/remotes/<remote>/HEAD`), falling back to
/// the branch's Oak parent and then `main`.
fn remote_default_branch(ctx: &RepoContext, remote: &str) -> Option<String> {
    git_stdout(
        &ctx.work_tree,
        &[
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .ok()
    .and_then(|r| r.strip_prefix(&format!("{remote}/")).map(str::to_string))
}

fn parent_branch(ctx: &RepoContext, repo: &dyn Repository, remote: &str, branch: &str) -> String {
    repo.get_branch(branch)
        .ok()
        .flatten()
        .and_then(|b| b.parent_branch)
        .or_else(|| remote_default_branch(ctx, remote))
        .unwrap_or_else(|| oak_core::DEFAULT_BRANCH.to_string())
}

fn remote_ref_exists(ctx: &RepoContext, remote: &str, branch: &str) -> bool {
    git_output(
        &ctx.work_tree,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}/{branch}"),
        ],
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// `https://github.com/<owner>/<repo>` for a GitHub remote URL, in any of the
/// usual SSH / HTTPS spellings.
pub fn github_web_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("https://github.com/{owner}/{name}"))
}

/// The page to review `branch`: a GitHub compare / new-PR page when the remote
/// is on GitHub and the branch isn't the base itself.
fn review_url(ctx: &RepoContext, remote: &str, branch: &str, base: &str) -> Option<String> {
    let web = github_web_url(&remote_url(ctx, remote)?)?;
    if branch == base {
        Some(format!("{web}/tree/{branch}"))
    } else {
        Some(format!("{web}/compare/{base}...{branch}?expand=1"))
    }
}

#[derive(Debug, Serialize)]
struct GitPushJson {
    schema_version: u32,
    backend: &'static str,
    pushed: bool,
    published: bool,
    remote_contacted: bool,
    remote: String,
    branch: String,
    pushed_head: Option<String>,
    review_url: Option<String>,
    recommended_next_commands: Vec<String>,
}

struct PushOutcome {
    remote: String,
    branch: String,
    head: Option<String>,
    review_url: Option<String>,
}

fn push_inner(ctx: &RepoContext, remote: Option<&str>, force: bool) -> Result<PushOutcome> {
    let repo = ctx.open()?;
    let branch = current_branch(repo.as_ref())?;
    let head = repo.get_branch_head(&branch)?.map(|h| h.0);
    if head.is_none() {
        return Err(OakError::InvalidArgument(format!(
            "Branch '{branch}' has no commits yet; run `oak commit` first"
        )));
    }
    let remote = resolve_remote(ctx, remote, &branch)?;
    let base = parent_branch(ctx, repo.as_ref(), &remote, &branch);
    drop(repo);

    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut args = vec!["push", "--set-upstream"];
    if force {
        // Never clobber work someone else pushed since our last fetch.
        args.push("--force-with-lease");
    }
    args.push(&remote);
    args.push(&refspec);
    if !git_streamed(&ctx.work_tree, &args)? {
        return Err(OakError::Git(format!(
            "`git push` to '{remote}' was rejected. If the remote moved, run `oak pull` and retry{}.",
            if force { "" } else { " (or `oak push --force` to overwrite with lease)" }
        )));
    }

    let review_url = review_url(ctx, &remote, &branch, &base);
    Ok(PushOutcome {
        remote,
        branch,
        head,
        review_url,
    })
}

/// `oak push` on a git-backed repo: `git push --set-upstream <remote> <branch>`.
pub fn push(ctx: &RepoContext, remote: Option<&str>, force: bool, json: bool) -> Result<()> {
    let outcome = push_inner(ctx, remote, force)?;
    if json {
        return output::print_json(&GitPushJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            backend: "git",
            pushed: true,
            published: true,
            remote_contacted: true,
            remote: outcome.remote,
            branch: outcome.branch,
            pushed_head: outcome.head,
            review_url: outcome.review_url,
            recommended_next_commands: vec!["oak status --json".to_string()],
        });
    }
    output::success(&format!(
        "Pushed '{}' to {}",
        outcome.branch, outcome.remote
    ));
    if let Some(url) = outcome.review_url {
        output::item(&format!("Open a pull request: {url}"));
    }
    Ok(())
}

/// `oak fetch` on a git-backed repo: `git fetch --prune <remote>`.
pub fn fetch(ctx: &RepoContext, remote: Option<&str>) -> Result<()> {
    let repo = ctx.open()?;
    let branch = repo.get_current_branch_name()?.unwrap_or_default();
    drop(repo);
    let remote = resolve_remote(ctx, remote, &branch)?;
    if !git_streamed(&ctx.work_tree, &["fetch", "--prune", &remote])? {
        return Err(OakError::Git(format!("`git fetch {remote}` failed")));
    }
    output::success(&format!("Fetched {remote}"));
    Ok(())
}

#[derive(Debug, Serialize)]
struct GitPullJson {
    schema_version: u32,
    backend: &'static str,
    remote: String,
    branch: String,
    parent: String,
    head_before: Option<String>,
    head_after: Option<String>,
    merged: Vec<String>,
    conflict_paths: Vec<String>,
}

/// `oak pull` on a git-backed repo: fetch, then bring the current branch up to
/// date with its own remote copy and then with its parent (the remote's
/// default branch), mirroring Oak's branch → parent pull order. Fast-forwards
/// when possible, otherwise records a git merge commit. On conflict, git's
/// standard merge state is left in place for the user to resolve.
pub fn pull(ctx: &RepoContext, remote: Option<&str>, json: bool) -> Result<()> {
    let repo = ctx.open()?;
    let branch = current_branch(repo.as_ref())?;
    let head_before = repo.get_head()?.map(|h| h.0);
    let remote = resolve_remote(ctx, remote, &branch)?;
    let parent = parent_branch(ctx, repo.as_ref(), &remote, &branch);
    drop(repo);

    if ctx.work_tree.join(".git").join("MERGE_HEAD").exists() {
        return Err(OakError::InvalidArgument(
            "A git merge is already in progress. Resolve the conflicts and run `git commit --no-edit`, or `git merge --abort`.".to_string(),
        ));
    }

    if !git_streamed(&ctx.work_tree, &["fetch", "--prune", &remote])? {
        return Err(OakError::Git(format!("`git fetch {remote}` failed")));
    }

    let mut targets = vec![branch.clone()];
    if parent != branch {
        targets.push(parent.clone());
    }
    let mut merged = Vec::new();
    for target in targets {
        if !remote_ref_exists(ctx, &remote, &target) {
            continue;
        }
        let upstream = format!("{remote}/{target}");
        let message = format!("Merge {upstream} into {branch}");
        let ok = git_streamed(
            &ctx.work_tree,
            &["merge", "--no-edit", "-m", &message, &upstream],
        )?;
        if !ok {
            let conflicts = git_stdout(&ctx.work_tree, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
            let conflict_paths: Vec<String> = conflicts.lines().map(str::to_string).collect();
            if conflict_paths.is_empty() {
                // Git refused before merging (e.g. local edits would be
                // overwritten); nothing to resolve, just report.
                return Err(OakError::Git(format!(
                    "`git merge {upstream}` refused to run; commit or stash local changes that overlap and retry"
                )));
            }
            if json {
                output::print_json(&GitPullJson {
                    schema_version: crate::work_state::SCHEMA_VERSION,
                    backend: "git",
                    remote: remote.clone(),
                    branch: branch.clone(),
                    parent: parent.clone(),
                    head_before: head_before.clone(),
                    head_after: head_before.clone(),
                    merged,
                    conflict_paths: conflict_paths.clone(),
                })?;
                output::mark_json_payload_emitted();
            } else {
                output::warning(&format!("Conflicts merging {upstream}:"));
                for path in &conflict_paths {
                    output::item(path);
                }
                output::item(
                    "Resolve them, then `git add <paths> && git commit --no-edit` (or `git merge --abort`).",
                );
            }
            return Err(OakError::MergeConflict(conflict_paths.len()));
        }
        merged.push(upstream);
    }

    let head_after = git_stdout(&ctx.work_tree, &["rev-parse", "HEAD"]).ok();
    if json {
        return output::print_json(&GitPullJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            backend: "git",
            remote,
            branch,
            parent,
            head_before,
            head_after,
            merged,
            conflict_paths: Vec::new(),
        });
    }
    if head_before == head_after {
        output::success(&format!("'{branch}' is up to date with {remote}"));
    } else {
        output::success(&format!("Updated '{branch}' from {}", merged.join(", ")));
    }
    Ok(())
}

/// `oak merge` has no git-host equivalent we can drive safely (it's a
/// CI-gated, server-side squash merge). Point at the host's review flow.
pub fn merge(ctx: &RepoContext) -> Result<()> {
    let repo = ctx.open()?;
    let branch = current_branch(repo.as_ref())?;
    let remote = resolve_remote(ctx, None, &branch).ok();
    let hint = remote
        .as_deref()
        .and_then(|r| {
            let base = parent_branch(ctx, repo.as_ref(), r, &branch);
            review_url(ctx, r, &branch, &base)
        })
        .map(|url| format!(" Open a pull request at {url}, or run `gh pr create --fill`."))
        .unwrap_or_else(|| {
            " Push the branch and open a pull request on your git host.".to_string()
        });
    Err(OakError::InvalidArgument(format!(
        "`oak merge` merges on an Oak server; this repository is git-backed. Run `oak push`, then merge through your host.{hint}"
    )))
}

/// `oak commit --push` on a git-backed repo.
pub fn push_after_commit(ctx: &RepoContext, quiet: bool) -> Result<()> {
    if quiet {
        output::begin_capture();
        let result = push_inner(ctx, None, false).map(|_| ());
        let _ = output::end_capture();
        result
    } else {
        push(ctx, None, false, false)
    }
}

/// Fail fast before `oak commit --push` creates a checkpoint it can't publish.
pub fn preflight_push(ctx: &RepoContext) -> Result<()> {
    let repo = ctx.open()?;
    let branch = current_branch(repo.as_ref())?;
    resolve_remote(ctx, None, &branch).map(|_| ())
}

/// `oak finish` on a git-backed repo: set the branch description, checkpoint
/// dirty work (the description becomes the git commit message), and push.
pub fn finish(ctx: &RepoContext, description: &str) -> Result<super::finish::FinishJson> {
    if description.trim().is_empty() {
        return Err(OakError::InvalidArgument(
            "oak finish requires a non-empty --desc or --desc-file".to_string(),
        ));
    }
    let repo = ctx.open()?;
    let branch = current_branch(repo.as_ref())?;
    resolve_remote(ctx, None, &branch)?;
    let head_before = repo.get_branch_head(&branch)?.map(|h| h.0);
    repo.edit_branch_description(&branch, description)?;
    drop(repo);

    let mut completed = vec!["description".to_string()];
    output::begin_capture();
    let commit = super::commit::run_with_options(
        &ctx.work_tree,
        super::commit::CommitOptions {
            no_verify: false,
            paths: Vec::new(),
            push: false,
            json: false,
            quiet: true,
        },
    );
    let _ = output::end_capture();
    // A clean tree is fine: commit reports "nothing to commit" as success.
    commit?;
    let head_mid = ctx.open()?.get_branch_head(&branch)?.map(|h| h.0);
    let committed = head_mid != head_before;
    completed.push("commit".to_string());

    output::begin_capture();
    let pushed = push_inner(ctx, None, false);
    let _ = output::end_capture();
    let pushed = pushed?;
    completed.push("push".to_string());

    Ok(super::finish::FinishJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        context: "git".to_string(),
        branch: branch.clone(),
        branch_description: description.to_string(),
        phase: "complete".to_string(),
        completed_phases: completed,
        pending_phases: Vec::new(),
        retry_command: None,
        manual_recovery_commands: Vec::new(),
        branch_url: pushed.review_url,
        head_before,
        head_after: pushed.head,
        committed,
        pushed: true,
        description_synced: true,
        unpushed_before: 0,
        unpushed_after: 0,
    })
}

/// `oak clone <git-url> --git`: a plain `git clone`, kept git-backed. Oak
/// commands then operate on the clone's `.git` directly — no import, and
/// `oak push` / `oak pull` go back to the same host.
pub fn clone(url: &str, dest: Option<PathBuf>, cwd: &Path) -> Result<()> {
    let dest =
        dest.unwrap_or_else(|| PathBuf::from(super::git_clone::derive_dest_dir_from_git_url(url)));
    let full_dest = if dest.is_absolute() {
        dest
    } else {
        cwd.join(dest)
    };
    output::info(&format!("Cloning git repository '{url}'..."));
    let status = Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(&full_dest)
        .status()
        .map_err(|e| {
            OakError::Git(format!(
                "failed to run `git clone` (is git installed and on PATH?): {e}"
            ))
        })?;
    if !status.success() {
        return Err(OakError::Git(format!(
            "`git clone` failed with status {status}"
        )));
    }
    output::success(&format!(
        "Cloned into {} (git-backed: oak commands read and write its .git directly)",
        full_dest.display()
    ));
    output::item("Start work with `oak switch -c <name>`; publish with `oak push`.");
    Ok(())
}

/// `oak init --git`: create a git repository (default branch `main`) that Oak
/// operates on directly.
pub fn init(path: &Path) -> Result<()> {
    if path.join(".git").exists() {
        return Err(OakError::RepoAlreadyExists);
    }
    std::fs::create_dir_all(path)?;
    let out = Command::new("git")
        .arg("init")
        .arg("--initial-branch")
        .arg(oak_core::DEFAULT_BRANCH)
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            OakError::Git(format!(
                "failed to run `git init` (is git installed and on PATH?): {e}"
            ))
        })?;
    if !out.status.success() {
        return Err(OakError::Git(format!(
            "`git init` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    output::success(&format!(
        "Initialized git-backed repository in {}",
        path.join(".git").display()
    ));
    output::item(
        "Link a host with `git remote add origin <url>`, then `oak commit` and `oak push`.",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::github_web_url;

    #[test]
    fn github_web_url_handles_common_spellings() {
        for url in [
            "git@github.com:acme/widgets.git",
            "git@github.com:acme/widgets",
            "ssh://git@github.com/acme/widgets.git",
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets/",
        ] {
            assert_eq!(
                github_web_url(url).as_deref(),
                Some("https://github.com/acme/widgets"),
                "{url}"
            );
        }
        assert_eq!(github_web_url("git@gitlab.com:acme/widgets.git"), None);
        assert_eq!(github_web_url("https://github.com/acme"), None);
    }
}
