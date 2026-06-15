use std::io::IsTerminal;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use dialoguer::{Confirm, Select};
use oak_core::{BranchStatus, Hash, Manifest, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::commands::commit::get_status;
use crate::output;
use crate::resolve::Backend;
use crate::workdir_lock::WorkdirLock;

const MAIN_REFRESH_CACHE_TTL_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreePolicy {
    Carry,
    Discard,
}

impl WorktreePolicy {
    pub fn from_clean_flag(clean: bool) -> Self {
        if clean {
            Self::Discard
        } else {
            Self::Carry
        }
    }
}

/// Switch to a branch or detach HEAD at a specific commit. When `name` is
/// `None` (no argument supplied), prompt the user to pick a branch
/// interactively.
pub fn run(path: &Path, name: Option<&str>, detach: bool) -> Result<()> {
    run_with_policy(path, name, detach, WorktreePolicy::Carry)
}

pub fn run_with_policy(
    path: &Path,
    name: Option<&str>,
    detach: bool,
    policy: WorktreePolicy,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    if name.is_none() && detach {
        return Err(OakError::Io(std::io::Error::other(
            "--detach requires a commit hash",
        )));
    }

    if name.is_none() && !is_interactive() {
        return Err(OakError::Io(std::io::Error::other(
            "oak switch needs a branch name in non-interactive mode. Use `oak switch -c` to create a generated branch from latest available main, `oak switch -c --clean` to start clean, or `oak switch NAME --clean` to switch to an existing branch with a clean working tree.",
        )));
    }

    // Check for uncommitted changes first
    let (changes, _, _) = get_status(path)?;
    if !changes.is_empty() && policy != WorktreePolicy::Discard {
        return Err(OakError::UncommittedChanges);
    }

    // Every path below that touches the working tree materializes a
    // manifest; hold the workdir lock for all of them.
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;

    let name = match name {
        Some(n) => n.to_string(),
        None => select_branch_interactive(repo.as_ref())?,
    };

    let name = name.as_str();

    // `main` only exists on the server. Locally users always work on a
    // feature/personal branch, so reject any attempt to switch to it.
    if !detach && name == "main" {
        return Err(OakError::Io(std::io::Error::other(
            "`main` only exists on the server — work on a personal/feature branch and let the server squash-merge your branch description onto main",
        )));
    }

    if detach {
        // --detach: treat name as a commit hash
        let hash = Hash::from_hex(name)?;
        let commit = repo
            .get_commit(&hash)?
            .ok_or_else(|| OakError::CommitNotFound(hash.to_string()))?;
        let manifest = repo
            .get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(oak_core::Manifest::empty);

        update_working_dir(&lock, root, repo.as_ref(), &manifest)?;

        repo.set_current_branch("")?;
        repo.set_head(&hash)?;

        output::success(&format!("HEAD is now at {} (detached)", hash.short()));
        if let Some(msg) = commit.message.as_deref() {
            output::info(&format!("  {msg}"));
        }
        return Ok(());
    }

    // Try to find as a branch. A row whose head was never materialized
    // (clone stores every open branch's metadata, but only the landing
    // branch's commits) is resolved from the remote first — switching onto
    // it blindly would materialize an empty manifest and needlessly wipe
    // the working tree.
    if repo.get_branch(name)?.is_some() {
        if repo.get_branch_head(name)?.is_none() {
            fetch_remote_branch(&ctx, &lock, name)?;
        }
        if try_switch_to_branch(&lock, root, repo.as_ref(), name)? {
            return Ok(());
        }
    }

    // Try to treat as a commit hash (detach automatically)
    let looks_like_hash = Hash::from_hex(name).is_ok();
    if let Ok(hash) = Hash::from_hex(name) {
        if let Some(commit) = repo.get_commit(&hash)? {
            let manifest = repo
                .get_manifest(&commit.manifest_hash)?
                .unwrap_or_else(oak_core::Manifest::empty);

            update_working_dir(&lock, root, repo.as_ref(), &manifest)?;

            repo.set_current_branch("")?;
            repo.set_head(&hash)?;

            output::success(&format!("HEAD is now at {} (detached)", hash.short()));
            if let Some(msg) = commit.message.as_deref() {
                output::info(&format!("  {msg}"));
            }
            return Ok(());
        }
    }

    // Not a local branch or commit. The branch may still exist on the
    // remote — pushed from another machine or created in the web UI —
    // so fetch it into local storage and switch onto it when found.
    if !looks_like_hash
        && fetch_remote_branch(&ctx, &lock, name)?
        && try_switch_to_branch(&lock, root, repo.as_ref(), name)?
    {
        return Ok(());
    }

    // Branch doesn't exist. If we're attached to a TTY, offer to create it
    // off either the current branch or the server's main branch. A
    // hash-shaped name is almost certainly a typo'd commit ref, not a new
    // branch — skip the prompt in that case.
    if !looks_like_hash && is_interactive() {
        // Drop the borrow on `repo` before re-opening through `branch::new_branch`,
        // and release the workdir lock — `new_branch` acquires its own.
        drop(repo);
        drop(lock);
        return prompt_create_and_switch(path, &ctx, name);
    }

    Err(OakError::BranchNotFound(format!(
        "'{name}' is not a branch or commit hash"
    )))
}

/// Switch onto an existing local branch: materialize its head manifest and
/// repoint the current-branch/HEAD metadata. Returns false when no branch
/// row named `name` exists.
fn try_switch_to_branch(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    name: &str,
) -> Result<bool> {
    let Some(branch) = repo.get_branch(name)? else {
        return Ok(false);
    };

    let head_hash = repo.get_branch_head(&branch.name)?;
    let manifest = if let Some(ref h) = head_hash {
        let commit = repo
            .get_commit(h)?
            .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
        repo.get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(oak_core::Manifest::empty)
    } else {
        oak_core::Manifest::empty()
    };

    update_working_dir(lock, root, repo, &manifest)?;

    repo.set_current_branch(&branch.name)?;
    if let Some(ref h) = head_hash {
        repo.set_head(h)?;
    }

    output::success(&format!("Switched to branch '{}'", branch.name));
    Ok(true)
}

/// `oak switch <name>` fallback when `<name>` isn't materialized locally —
/// no branch row at all, or a metadata-only row without a head (clone
/// stores every open branch's metadata but only the landing branch's
/// commits). Looks for the branch on the remote (pushed from another
/// machine or created in the web UI), pulls its commits + content, and
/// materializes the local row + head so the normal switch path can take
/// over.
///
/// Returns false when there's no remote to ask or the remote doesn't know
/// the branch — including network failures, which degrade to a warning so
/// the caller can fall through to its create-or-error path. Errors only
/// when the branch *does* exist remotely but can't be switched onto safely.
fn fetch_remote_branch(
    ctx: &crate::resolve::RepoContext,
    lock: &WorkdirLock,
    name: &str,
) -> Result<bool> {
    // Only the SQLite backend can ingest a pull response.
    if !matches!(ctx.backend, Backend::Sqlite) {
        return Ok(false);
    }
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let Some(remote) = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .map(|r| r.trim_end_matches('/').to_string())
        .filter(|r| !r.is_empty())
    else {
        return Ok(false);
    };
    let (owner, repo_name) = super::read_repo_identity(&repo)?;
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    output::info(&format!("Fetching branch '{name}' from {remote}…"));

    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
    match rt.block_on(super::pull::pull_async(
        lock,
        &repo,
        &remote,
        &format!("{owner}/{repo_name}/pull"),
        Some(name),
        None,
        false,
        &ctx.work_tree,
        api_key.as_deref(),
    )) {
        Ok(()) => {}
        Err(OakError::RemoteRepoNotFound(_)) => return Ok(false),
        Err(e) => {
            output::warning(&format!("Could not check the remote for '{name}': {e}"));
            return Ok(false);
        }
    }

    let Some(branch) = repo.get_branch(name)? else {
        // The remote doesn't know the branch either.
        return Ok(false);
    };
    if branch.status == BranchStatus::Closed {
        // Already merged/closed on the server — don't resurrect it locally.
        let _ = repo.delete_branch(name);
        return Err(OakError::BranchNotFound(format!(
            "'{name}' was already merged on the remote"
        )));
    }

    // A branch with no commits of its own is seeded at a main commit, so
    // the pull above couldn't learn its head from commit rows. Resolve the
    // head via the branch-head endpoint, refreshing main when the seed
    // commit isn't local yet.
    if repo.get_branch_head(name)?.is_none() {
        let client = crate::http::api_client();
        let head = rt
            .block_on(super::push::fetch_remote_branch_head(
                &client,
                &remote,
                &format!("{owner}/{repo_name}"),
                name,
                api_key.as_deref(),
            ))?
            .ok_or_else(|| {
                OakError::Server(format!(
                    "branch '{name}' exists on the remote but has no head commit"
                ))
            })?;
        if repo.get_commit(&head)?.is_none() {
            // The seed is usually a main commit we haven't pulled yet.
            rt.block_on(super::sync::fetch_parent_from_server(&repo, "main"))?;
        }
        let materializable = match repo.get_commit(&head)? {
            Some(c) => repo.get_manifest(&c.manifest_hash)?.is_some(),
            None => false,
        };
        if !materializable {
            // Don't leave a head-less row behind: a later `oak switch` would
            // treat it as an empty branch and wipe the working tree.
            let _ = repo.delete_branch(name);
            return Err(OakError::Server(format!(
                "branch '{name}' is seeded at commit {} which isn't available locally — \
                 commit on the branch from the machine that created it, then retry",
                head.short()
            )));
        }
        repo.set_branch_head(name, &head)?;
    }

    Ok(true)
}

/// Create a branch and switch to it. This is the implementation behind
/// `oak switch -c <name>`.
pub fn create(path: &Path, name: &str, policy: WorktreePolicy) -> Result<()> {
    create_from_main(path, Some(name), policy)
}

/// Create a generated branch from latest available `main` and switch to it.
///
/// This is the agent-fast path behind `oak switch -c` with no name. If `main`
/// was refreshed recently, use the local head immediately; otherwise try a
/// best-effort remote refresh and fall back to local `main` when offline.
pub fn fresh(path: &Path, policy: WorktreePolicy) -> Result<()> {
    create_from_main(path, None, policy)
}

fn create_from_main(path: &Path, name: Option<&str>, policy: WorktreePolicy) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let root = ctx.work_tree.clone();
    let discard = policy == WorktreePolicy::Discard;

    let repo_probe = ctx.open()?;
    let has_remote = repo_probe
        .get_metadata(MetadataKey::RemoteUrl)?
        .map(|remote| !remote.trim().is_empty())
        .unwrap_or(false);
    let local_main_head = repo_probe.get_branch_head(oak_core::DEFAULT_BRANCH)?;
    let current_branch_head = match repo_probe.get_current_branch_name()? {
        Some(branch) => repo_probe.get_branch_head(&branch)?,
        None => None,
    };
    drop(repo_probe);

    // Dirtiness must be probed BEFORE the main refresh below: fetching can
    // move HEAD, which would make the unchanged worktree read as dirty and
    // wrongly skip materialization.
    let dirty = if !discard && (has_remote || local_main_head.is_some()) {
        let (changes, _, _) = get_status(path)?;
        !changes.is_empty()
    } else {
        false
    };
    let main_head = latest_main_head(&ctx)?;

    // `main` can legitimately have no commits: a local-only repo can never
    // merge into it (merge needs a remote), and a freshly created server
    // repo has no main history yet. Seeding the new branch at "nothing"
    // would re-present every committed file as work to commit — the last
    // commit visually evaporates from status, and the next snapshot
    // re-records the whole tree. Seed from the current branch's head
    // instead (the `git checkout -b` expectation); the branch still
    // parents onto `main`, so a later merge squashes the full chain as
    // usual. The worktree already IS that state (plus any dirty edits we
    // want to carry), so this path needs no status probe and no
    // materialization — branch creation stays O(1) in tree size.
    let seeded_from_current = main_head.is_none() && current_branch_head.is_some();
    let seed_head = main_head.or(current_branch_head);

    let repo = ctx.open()?;
    let manifest = manifest_for_head(repo.as_ref(), seed_head.as_ref())?;
    let branch_name = match name {
        Some(name) => {
            if name == oak_core::DEFAULT_BRANCH {
                return Err(OakError::Io(std::io::Error::other(
                    "`main` only exists on the server — create a personal/feature branch instead",
                )));
            }
            if repo.get_branch(name)?.is_some() {
                return Err(OakError::BranchAlreadyExists(name.to_string()));
            }
            name.to_string()
        }
        None => super::merge::next_open_personal_branch_name(repo.as_ref())?,
    };
    let branch = oak_core::Branch::new(
        branch_name.clone(),
        None,
        Some(oak_core::DEFAULT_BRANCH.to_string()),
    );

    let materialize = discard || (!dirty && !seeded_from_current && seed_head.is_some());
    if materialize {
        let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
        update_working_dir(&lock, &root, repo.as_ref(), &manifest)?;
    }

    repo.store_branch(&branch)?;
    if let Some(ref head) = seed_head {
        repo.set_branch_head(&branch_name, head)?;
        repo.set_head(head)?;
    }
    repo.set_current_branch(&branch_name)?;

    // Preserving the worktree changes branch metadata only. Leave the stat
    // cache in place and let the next status/diff/commit validate paths with
    // the normal `(mtime, ctime, size)` guard instead of rehashing everything
    // during branch creation.

    if std::io::stdout().is_terminal() {
        output::success(&format!("Created branch '{branch_name}' (parent: 'main')"));
    } else {
        // Branches always parent onto main (the flat model), so "from 'main'"
        // carries no information for a piped reader — drop it.
        output::print_line(&format!("Created branch '{branch_name}'"));
    }

    Ok(())
}

fn latest_main_head(ctx: &crate::resolve::RepoContext) -> Result<Option<Hash>> {
    let repo = ctx.open()?;
    let has_remote = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .map(|remote| !remote.trim().is_empty())
        .unwrap_or(false);

    if !has_remote {
        return repo.get_branch_head(oak_core::DEFAULT_BRANCH);
    }

    if main_refresh_is_recent(repo.as_ref())? {
        return repo.get_branch_head(oak_core::DEFAULT_BRANCH);
    }
    drop(repo);

    match &ctx.backend {
        Backend::Sqlite => {
            let db_path = ctx.db_path()?;
            let sqlite_repo = SqliteRepository::open(&db_path)?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            match rt.block_on(super::sync::fetch_parent_from_server(
                &sqlite_repo,
                oak_core::DEFAULT_BRANCH,
            )) {
                Ok(head) => Ok(head),
                Err(err) => {
                    output::warning(&format!(
                        "Could not refresh main from remote ({err}); using local main"
                    ));
                    sqlite_repo.get_branch_head(oak_core::DEFAULT_BRANCH)
                }
            }
        }
        Backend::Git { .. } => {
            output::warning("Remote refresh for git-backed repos is unsupported; using local main");
            let repo = ctx.open()?;
            repo.get_branch_head(oak_core::DEFAULT_BRANCH)
        }
    }
}

fn main_refresh_is_recent(repo: &dyn Repository) -> Result<bool> {
    let Some(raw) = repo.get_metadata(MetadataKey::MainLastCheckedAt)? else {
        return Ok(false);
    };
    let Ok(checked_at) = raw.parse::<u64>() else {
        return Ok(false);
    };
    Ok(unix_now_secs().saturating_sub(checked_at) <= MAIN_REFRESH_CACHE_TTL_SECS)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn manifest_for_head(repo: &dyn Repository, head: Option<&Hash>) -> Result<Manifest> {
    let Some(head) = head else {
        return Ok(Manifest::empty());
    };

    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    Ok(repo
        .get_manifest(&commit.manifest_hash)?
        .unwrap_or_else(Manifest::empty))
}

fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Branches the interactive picker should offer: everything except `main`.
/// `main` only exists on the server and `oak switch main` is always rejected
/// (see `run_with_policy`), so listing it only invites a guaranteed error and
/// pads the picker with a row that "looks open" but can't be selected.
fn pickable_branches(branches: Vec<oak_core::Branch>) -> Vec<oak_core::Branch> {
    branches
        .into_iter()
        .filter(|b| b.name != oak_core::DEFAULT_BRANCH)
        .collect()
}

/// Prompt the user to pick a branch with `dialoguer::Select`. Used when
/// `oak switch` is invoked without a name argument in an interactive terminal.
fn select_branch_interactive(repo: &dyn Repository) -> Result<String> {
    // Reap branches already merged into the *local* `main` first, so the picker
    // doesn't list phantom rows that still read `[open]` for work that's
    // actually landed — the same cleanup `oak pull` performs. This is offline
    // and idempotent; branches merged on the server but not yet pulled keep
    // showing until the next `oak pull` learns main moved.
    let pruned = super::merge::prune_merged_branches(repo)?;
    super::merge::print_pruned_branches(&pruned);

    let branches = pickable_branches(repo.list_branches()?);
    if branches.is_empty() {
        return Err(OakError::Io(std::io::Error::other(
            "no branches to switch to (only `main` exists, which lives on the server)",
        )));
    }
    let current = repo.get_current_branch_name().ok().flatten();

    let mut items = Vec::with_capacity(branches.len());
    let mut default_idx = 0;
    for (i, br) in branches.iter().enumerate() {
        let is_current = current.as_deref() == Some(&br.name);
        if is_current {
            default_idx = i;
        }
        let marker = if is_current { "* " } else { "  " };
        let status = br.status.as_str();
        let head_short = repo
            .get_branch_head(&br.name)
            .ok()
            .flatten()
            .map(|h| h.short().to_string())
            .unwrap_or_else(|| "-".to_string());
        let mut line = format!("{}{}  [{}]  {}", marker, br.name, status, head_short);
        if let Some(ref desc) = br.description {
            line.push_str(&format!(" - {desc}"));
        }
        items.push(line);
    }

    let idx = Select::new()
        .with_prompt("Switch to which branch?")
        .items(&items)
        .default(default_idx)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    Ok(branches[idx].name.clone())
}

/// Prompt the user to create the missing branch and pick its parent
/// (current branch vs server's `main`). Used by `oak switch <name>` when
/// `<name>` doesn't exist yet.
fn prompt_create_and_switch(
    path: &Path,
    ctx: &crate::resolve::RepoContext,
    name: &str,
) -> Result<()> {
    let create = Confirm::new()
        .with_prompt(format!("Branch '{name}' doesn't exist. Create it?"))
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    if !create {
        return Err(OakError::BranchNotFound(format!(
            "'{name}' is not a branch or commit hash"
        )));
    }

    // Re-open the repo to read current branch + remote metadata. Cheap; the
    // db handle is just an in-process file lock.
    let repo = ctx.open()?;
    let current = repo.get_current_branch_name().ok().flatten();
    let has_remote = repo
        .get_metadata(MetadataKey::RemoteUrl)
        .ok()
        .flatten()
        .is_some();
    // Only the SQLite backend can materialize main from the server today —
    // that path needs a `SqliteRepository` to write commit/manifest/blob rows.
    let can_fetch_main = has_remote && matches!(ctx.backend, Backend::Sqlite);
    drop(repo);

    enum ParentKind {
        Current,
        ServerMain,
    }

    let mut options: Vec<String> = Vec::new();
    let mut kinds: Vec<ParentKind> = Vec::new();
    if let Some(ref c) = current {
        options.push(format!("Current branch ('{c}')"));
        kinds.push(ParentKind::Current);
    }
    if can_fetch_main {
        options.push("Server's main branch".to_string());
        kinds.push(ParentKind::ServerMain);
    }

    if options.is_empty() {
        return Err(OakError::Io(std::io::Error::other(
            "no parent available — not on a branch and no remote configured",
        )));
    }

    let idx = if options.len() == 1 {
        0
    } else {
        Select::new()
            .with_prompt("Branch off of?")
            .items(&options)
            .default(0)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?
    };

    match kinds[idx] {
        ParentKind::Current => super::branch::new_branch(path, name, None, None, None),
        ParentKind::ServerMain => {
            // Fetch main's head + manifest + missing blobs into local
            // storage, then create the branch parented onto `main` and
            // seeded at that head — `branch::new_branch` will rewrite the
            // working tree to match.
            let db_path = ctx.db_path()?;
            let sqlite_repo = SqliteRepository::open(&db_path)?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            let head = rt
                .block_on(super::sync::fetch_parent_from_server(&sqlite_repo, "main"))?
                .ok_or_else(|| OakError::Server("Server has no commits on main yet".to_string()))?;
            drop(sqlite_repo);
            super::branch::new_branch(path, name, None, Some(head.as_str()), Some("main"))
        }
    }
}

/// Update the working directory to fully match a manifest: write every entry,
/// delete untracked leftovers, prune empty dirs, refresh the stat cache.
/// Thin wrapper over [`crate::materialize::apply_manifest`] — the single
/// shared materializer. Errors on a missing blob rather than silently
/// skipping it (a skip leaves a partial tree that the next unattended
/// `oak commit` would record).
pub fn update_working_dir(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    manifest: &oak_core::Manifest,
) -> Result<()> {
    crate::materialize::apply_manifest(
        lock,
        root,
        repo,
        manifest,
        crate::materialize::ApplyOpts::default(),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::Branch;

    fn feature(name: &str) -> Branch {
        Branch::new(name.to_string(), None, Some("main".to_string()))
    }

    #[test]
    fn pickable_branches_drops_main() {
        let branches = vec![
            Branch::new("main".to_string(), None, None),
            feature("zdgeier-aaa111"),
            feature("zdgeier-bbb222"),
        ];
        let names: Vec<String> = pickable_branches(branches)
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(names, vec!["zdgeier-aaa111", "zdgeier-bbb222"]);
    }

    #[test]
    fn pickable_branches_keeps_features_without_main_row() {
        let branches = vec![feature("zdgeier-aaa111")];
        assert_eq!(pickable_branches(branches).len(), 1);
    }

    #[test]
    fn pickable_branches_empty_when_only_main() {
        let branches = vec![Branch::new("main".to_string(), None, None)];
        assert!(pickable_branches(branches).is_empty());
    }
}
