mod journal;
pub(crate) use journal::ensure_no_pending;

use std::io::IsTerminal;
use std::path::Path;

use dialoguer::{Confirm, Select};
use oak_core::{BranchStatus, Hash, Manifest, MetadataKey, OakError, Result, Tree};
use oak_core::{Repository, SqliteRepository};

use crate::commands::commit::worktree_is_clean_without_storing_blobs;
use crate::commands::merge_tree::{Removed, WorktreeDiff};
use crate::materialize::{ApplyReport, MissingBlobs};
use crate::output;
use crate::remote_head::{
    self, ObserveOptions, ObservePolicy, RemoteHeadObservation, RemoteHeadSource,
};
use crate::resolve::Backend;
use crate::workdir_lock::WorkdirLock;

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
    let repo = ctx.open_for_switch()?;

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

    // Every path below that touches the working tree materializes a
    // manifest; hold the workdir lock for all of them, from the dirty check
    // on, so no other Oak writer moves the tree between the two.
    let lock = WorkdirLock::acquire_for_switch(&ctx.oak_dir)?;
    if journal::resume(&ctx, &lock, repo.as_ref(), name)? {
        return Ok(());
    }

    // Check for uncommitted changes first (`--clean` discards them). The
    // clean check proves a clean tree by tree hash, without flattening HEAD.
    // A clean tree holds the snapshot the check compared against, so the
    // switch writes only the files that differ from it; `--clean` rewrites
    // the whole tree.
    let holds = if policy == WorktreePolicy::Discard {
        WorktreeHolds::Unknown
    } else {
        if !worktree_is_clean_without_storing_blobs(repo.as_ref(), root)? {
            return Err(OakError::UncommittedChanges);
        }
        WorktreeHolds::after_clean_guard(repo.as_ref())?
    };

    let name = match name {
        Some(n) => n.to_string(),
        None => select_branch_interactive(repo.as_ref())?,
    };

    let name = name.as_str();

    // `main` only exists on the server. Locally users always work on a
    // feature/personal branch, so reject any attempt to switch to it. A
    // git-backed repo is the exception: there `main` is an ordinary local
    // branch that `oak pull` keeps current and the git host merges into.
    let git_backed = matches!(ctx.backend, crate::resolve::Backend::Git { .. });
    if !detach && name == "main" && !git_backed {
        return Err(OakError::TrunkIsServerOnly(
            "`main` only exists on the server — work on a personal/feature branch and let the server squash-merge your branch description onto main".to_string(),
        ));
    }

    if detach {
        // --detach: treat name as a commit hash
        let hash = Hash::from_hex(name)?;
        let commit = repo
            .get_commit(&hash)?
            .ok_or_else(|| OakError::CommitNotFound(hash.to_string()))?;
        let journal = switch_working_dir(
            &ctx,
            &lock,
            root,
            repo.as_ref(),
            &holds,
            &commit.manifest_hash,
            name,
            "",
            Some(hash.clone()),
        )?;
        journal.finish(&ctx, repo.as_ref())?;

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
        if try_switch_to_branch(&ctx, &lock, root, repo.as_ref(), &holds, name)? {
            return Ok(());
        }
    }

    // Try to treat as a commit hash (detach automatically)
    let looks_like_hash = Hash::from_hex(name).is_ok();
    if let Ok(hash) = Hash::from_hex(name) {
        if let Some(commit) = repo.get_commit(&hash)? {
            let journal = switch_working_dir(
                &ctx,
                &lock,
                root,
                repo.as_ref(),
                &holds,
                &commit.manifest_hash,
                name,
                "",
                Some(hash.clone()),
            )?;
            journal.finish(&ctx, repo.as_ref())?;

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
        && try_switch_to_branch(&ctx, &lock, root, repo.as_ref(), &holds, name)?
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

/// Switch onto an existing local branch: move the working tree to its head's
/// snapshot and repoint the current-branch/HEAD metadata. Returns false when
/// no branch row named `name` exists.
fn try_switch_to_branch(
    ctx: &crate::resolve::RepoContext,
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    holds: &WorktreeHolds,
    name: &str,
) -> Result<bool> {
    let Some(branch) = repo.get_branch(name)? else {
        return Ok(false);
    };

    let head_hash = repo.get_branch_head(&branch.name)?;
    let target_root = root_for_head(repo, head_hash.as_ref())?;
    let journal = switch_working_dir(
        ctx,
        lock,
        root,
        repo,
        holds,
        &target_root,
        &branch.name,
        &branch.name,
        head_hash,
    )?;
    journal.finish(ctx, repo)?;

    output::success(&format!("Switched to branch '{}'", branch.name));
    Ok(true)
}

/// Move the working tree to the snapshot `to` for `oak switch`, first
/// fetching from the remote the blobs the move writes that aren't local.
#[allow(clippy::too_many_arguments)]
fn switch_working_dir(
    ctx: &crate::resolve::RepoContext,
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    holds: &WorktreeHolds,
    to: &Hash,
    target: &str,
    target_name: &str,
    target_head: Option<Hash>,
) -> Result<journal::Journal> {
    let move_plan = WorkingDirMove::plan(repo, holds, to)?;
    // Only the files the move writes need their blobs: a file it leaves
    // alone is already on disk, unchanged.
    ensure_manifest_blobs_available(ctx, move_plan.files(), target)?;
    let journal = journal::Journal::begin(ctx, repo, &move_plan, target_name, target_head, None)?;
    move_plan.apply(lock, root, repo)?.finish();
    Ok(journal)
}

fn ensure_manifest_blobs_available(
    ctx: &crate::resolve::RepoContext,
    manifest: &Manifest,
    target: &str,
) -> Result<()> {
    if !matches!(ctx.backend, Backend::Sqlite) {
        return Ok(());
    }

    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    // Blobs the server withheld under path-based permissions can't be
    // hydrated — leave them out of the fetch (the working-dir update skips
    // their files with an access note) instead of failing the whole switch.
    let restricted = super::restricted::load_restricted_blobs(&repo);
    let known_lost = super::known_loss::load_known_lost_blobs(&repo);
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(MetadataKey::SparsePaths)?.as_deref(),
    );
    let mut missing = Vec::new();
    for entry in &manifest.entries {
        if cone.as_ref().is_some_and(|cone| !cone.covers(&entry.path))
            || repo.has_blob(&entry.blob_hash)?
            || restricted.contains(entry.blob_hash.as_str())
            || known_lost.contains(entry.blob_hash.as_str())
        {
            continue;
        }
        missing.push(entry.blob_hash.clone());
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    missing.dedup();

    let count = missing.len();
    let first = missing
        .first()
        .map(|h| h.as_str().to_string())
        .unwrap_or_default();
    let Some(remote) = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .map(|r| r.trim_end_matches('/').to_string())
        .filter(|r| !r.is_empty())
    else {
        return Err(OakError::Server(format!(
            "branch '{target}' references {count} blob(s) missing locally and no remote is configured to hydrate them. First missing: {first}"
        )));
    };

    let (owner, repo_name) = super::read_repo_identity(&repo)?;
    let api_key = super::credentials::effective_token_for_repository(&remote, &repo);

    output::info(&format!(
        "Fetching {count} missing blob(s) for '{target}' from {remote}…"
    ));
    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
    rt.block_on(super::blob_fetch::ensure_blobs_local(
        &repo,
        &remote,
        &owner,
        &repo_name,
        api_key.as_deref(),
        &missing,
    ))
    .map_err(|e| {
        OakError::Server(format!(
            "branch '{target}' references {count} blob(s) missing locally, and Oak could not hydrate them from the remote. First missing: {first}. Cause: {e}"
        ))
    })?;

    Ok(())
}

/// `oak switch <name>` fallback when `<name>` isn't materialized locally —
/// no branch row at all, or a metadata-only row without a head (clone
/// stores every open branch's metadata but only the landing branch's
/// commits). Looks for the branch on the remote (pushed from another
/// machine or created in the web UI), pulls its commits + content, and
/// materializes the local row + head so the normal switch path can take
/// over.
///
/// The pull only fetches: it leaves the working tree, HEAD and the current
/// branch alone, and the switch's own move then writes just the files that
/// differ from the snapshot the dirty guard proved (the pull has no local
/// head to diff from, so applying it would write every file of the fetched
/// head). With no local head the pull sends no `since`, so it can't reach
/// the 409 / divergence re-parent paths, which act only on the checked-out
/// branch anyway.
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
    let api_key = super::credentials::effective_token_for_repository(&remote, &repo);

    output::info(&format!("Fetching branch '{name}' from {remote}…"));

    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
    match rt.block_on(super::pull::pull_async_with_stats(
        lock,
        &repo,
        &remote,
        &format!("{owner}/{repo_name}/pull"),
        Some(name),
        None,
        false,
        &ctx.work_tree,
        api_key.as_deref(),
        true,
        super::pull::PullWorktreeMode::DeferToSwitch,
    )) {
        Ok(_) => {}
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
            // The seed is usually main's head, which a head-only refresh
            // stores. An older seed is only reachable through main's
            // ancestry, so fall back to the complete backfill for it.
            rt.block_on(super::sync::fetch_parent_from_server(
                &repo,
                "main",
                super::sync::AncestryNeed::HeadOnly,
            ))?;
            if repo.get_commit(&head)?.is_none() {
                rt.block_on(super::sync::fetch_parent_from_server(
                    &repo,
                    "main",
                    super::sync::AncestryNeed::Complete,
                ))?;
            }
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

/// What branch creation did: the branch, and the `main` it started from with
/// how that head is known. The base fields of the `oak start --json` receipt
/// are rendered from this.
#[derive(Debug, Clone)]
pub struct BranchCreated {
    pub branch: String,
    /// What the branch actually starts from.
    pub base: BranchBase,
}

/// The commit a new branch starts at, and how that commit is known.
#[derive(Debug, Clone)]
pub enum BranchBase {
    /// `main`, as observed for this creation. `head` is `None` only when
    /// `main` has no commits and there was no current head to seed from
    /// either: the branch starts empty.
    Main(RemoteHeadObservation),
    /// `main` has no commits yet (verified or not, per `main`), so the
    /// branch was seeded at the current branch's local head instead. It
    /// still parents onto `main`.
    CurrentBranch {
        branch: Option<String>,
        head: Hash,
        main: RemoteHeadObservation,
    },
}

/// Create a branch and switch to it. This is the implementation behind
/// `oak switch -c <name>`.
pub fn create(path: &Path, name: &str, policy: WorktreePolicy) -> Result<BranchCreated> {
    create_from_main(path, Some(name), policy)
}

/// Create a generated branch from latest `main` and switch to it.
///
/// Branch creation always verifies `main`'s head against the remote (one
/// request; `main` is fetched only when it moved). When the remote can't be
/// reached the branch starts from local `main` and says so.
pub fn fresh(path: &Path, policy: WorktreePolicy) -> Result<BranchCreated> {
    create_from_main(path, None, policy)
}

fn create_from_main(
    path: &Path,
    name: Option<&str>,
    policy: WorktreePolicy,
) -> Result<BranchCreated> {
    // A refusal to overwrite local work is advised as a branch creation's:
    // the edit conflicts with the newer main the branch would start from.
    create_from_main_inner(path, name, policy)
        .map_err(|error| error.in_transition(oak_core::WorktreeTransition::CreateBranch))
}

fn create_from_main_inner(
    path: &Path,
    name: Option<&str>,
    policy: WorktreePolicy,
) -> Result<BranchCreated> {
    let ctx = crate::resolve::resolve(path)?;
    // Branch creation changes the current branch and HEAD even when carrying
    // the worktree without materializing files. Serialize the entire state
    // read/refresh/transition with commit and other cooperative CLI writers.
    // Keep this guard through the reference writes, not just materialization.
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = ctx.work_tree.clone();
    let discard = policy == WorktreePolicy::Discard;

    let repo_probe = ctx.open()?;
    let current_branch_name = repo_probe.get_current_branch_name()?;
    let current_branch_head = match current_branch_name.as_deref() {
        Some(branch) => repo_probe.get_branch_head(branch)?,
        None => None,
    };

    // What the working tree holds is read BEFORE the main refresh below:
    // fetching can move HEAD, and the move to main is planned from the
    // snapshot the files on disk were checked out from. Uncommitted work is
    // carried by that plan — a file main did not change keeps the edit, a
    // file only main changed takes main's content, and a file both changed
    // refuses the creation (see `worktree_update::classify`) — so no
    // dirtiness probe is needed, and nothing is ever left at an older
    // main's content under a branch that starts at the newer one.
    let holds = if discard {
        WorktreeHolds::Unknown
    } else {
        match super::commit::clean_worktree_root(repo_probe.as_ref())? {
            Some(root) => WorktreeHolds::Snapshot(root),
            None => WorktreeHolds::Unknown,
        }
    };
    let head_before_refresh = repo_probe.get_head()?;
    drop(repo_probe);
    let base = observe_main_for_branch_creation(&ctx)?;
    let main_head = base.head.clone();

    // `main` can legitimately have no commits: a local-only repo can never
    // merge into it (merge needs a remote), and a freshly created server
    // repo has no main history yet. Seeding the new branch at "nothing"
    // would re-present every committed file as work to commit — the last
    // commit visually evaporates from status, and the next snapshot
    // re-records the whole tree. Seed from the current branch's head
    // instead (the `git checkout -b` expectation); the branch still
    // parents onto `main`, so a later merge squashes the full chain as
    // usual. The worktree already IS that state (plus any dirty edits we
    // want to carry), so this path moves no file.
    let seeded_from_current = main_head.is_none() && current_branch_head.is_some();
    let seed_head = main_head.or(current_branch_head);

    let repo = ctx.open()?;
    let branch_name = match name {
        Some(name) => {
            super::branch::validate_branch_name(name)?;
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

    let materialize = discard || (!seeded_from_current && seed_head.is_some());
    let journal = if materialize {
        let seed_root = root_for_head(repo.as_ref(), seed_head.as_ref())?;
        let plan = WorkingDirMove::plan(repo.as_ref(), &holds, &seed_root)?;
        // The move could not be planned as a diff from the snapshot the
        // tree was checked out from (that snapshot, or a tree on the way,
        // is not stored), so nothing says which files are the user's
        // edits. A whole-tree rewrite is then right only for a tree proved
        // clean — against the HEAD it was checked out from. A HEAD snapshot
        // that is itself incomplete locally can't prove the tree clean, so
        // it refuses the same way (fail closed), never with a storage error.
        if !discard
            && plan.rewrites_whole_tree()
            && (repo.get_head()? != head_before_refresh
                || !match worktree_is_clean_without_storing_blobs(repo.as_ref(), &root) {
                    Ok(clean) => clean,
                    Err(OakError::ManifestNotFound(_)) => false,
                    Err(error) => return Err(error),
                })
        {
            return Err(OakError::UncommittedChanges);
        }
        let journal = journal::Journal::begin(
            &ctx,
            repo.as_ref(),
            &plan,
            &branch_name,
            seed_head.clone(),
            Some(branch.clone()),
        )?;
        plan.apply(&lock, &root, repo.as_ref())?.finish();
        Some(journal)
    } else {
        None
    };

    if let Some(journal) = journal {
        journal.finish(&ctx, repo.as_ref())?;
    } else {
        repo.store_branch(&branch)?;
        if let Some(ref head) = seed_head {
            repo.set_branch_head(&branch_name, head)?;
            repo.set_head(head)?;
        }
        repo.set_current_branch(&branch_name)?;
    }

    if std::io::stdout().is_terminal() {
        output::success(&format!("Created branch '{branch_name}' (parent: 'main')"));
    } else {
        // `oak switch -c` always parents onto main (stacking is the separate
        // `--stack` path), so "from 'main'" carries no information for a
        // piped reader — drop it.
        output::print_line(&format!("Created branch '{branch_name}'"));
    }

    Ok(BranchCreated {
        branch: branch_name,
        base: match (seeded_from_current, seed_head) {
            (true, Some(head)) => BranchBase::CurrentBranch {
                branch: current_branch_name,
                head,
                main: base,
            },
            (true, None) | (false, _) => BranchBase::Main(base),
        },
    })
}

/// Which branch `oak switch -c --stack` / `--parent` stacks onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackParent {
    /// `--stack`: the current branch.
    Current,
    /// `--parent BRANCH`.
    Named(String),
}

impl StackParent {
    /// `None` means an ordinary, unstacked branch off main (including an
    /// explicit `--parent main`).
    pub fn from_flags(stack: bool, parent: Option<String>) -> Option<Self> {
        match parent {
            Some(p) if p == oak_core::DEFAULT_BRANCH => None,
            Some(p) => Some(Self::Named(p)),
            None if stack => Some(Self::Current),
            None => None,
        }
    }
}

/// Look up a local branch row for [`oak_core::validate_stack_parent`].
pub fn local_stack_node(repo: &dyn Repository, name: &str) -> Option<oak_core::StackNode> {
    repo.get_branch(name)
        .ok()
        .flatten()
        .map(|b| oak_core::StackNode {
            parent_branch: b.parent_branch,
            open: b.status == BranchStatus::Open,
        })
}

/// The chain of parents from `branch` down to the trunk, tip first:
/// `["b", "a", "main"]` for `main <- a <- b`. Stops at a branch that isn't
/// known locally, and is bounded by [`oak_core::MAX_STACK_DEPTH`].
pub fn stack_chain(repo: &dyn Repository, branch: &str) -> Vec<String> {
    let mut chain = vec![branch.to_string()];
    let mut next = repo
        .get_branch(branch)
        .ok()
        .flatten()
        .and_then(|b| b.parent_branch);
    while let Some(name) = next {
        if chain.contains(&name) || chain.len() > oak_core::MAX_STACK_DEPTH {
            break;
        }
        next = if name == oak_core::DEFAULT_BRANCH {
            None
        } else {
            repo.get_branch(&name)
                .ok()
                .flatten()
                .and_then(|b| b.parent_branch)
        };
        chain.push(name);
    }
    chain
}

/// The stack to show for `branch`, trunk first (`["main", "a", "b"]`), or
/// `None` when it isn't stacked (it parents straight onto main, or has no
/// parent).
pub fn stack_for_display(repo: &dyn Repository, branch: &str) -> Option<Vec<String>> {
    let mut chain = stack_chain(repo, branch);
    if chain.len() < 3 {
        return None;
    }
    chain.reverse();
    Some(chain)
}

/// Create a branch stacked on another open branch and switch to it. This is
/// `oak switch -c [NAME] --stack` (stack on the current branch) and
/// `oak switch -c [NAME] --parent BRANCH`.
///
/// The new branch starts at the parent's head and records the parent as its
/// `parent_branch`, so `oak pull` syncs from the parent, `oak diff --branch`
/// shows only this branch's own work, and the server restacks it onto the
/// parent's parent once the parent lands.
pub fn create_stacked(
    path: &Path,
    name: Option<&str>,
    parent: StackParent,
    policy: WorktreePolicy,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = ctx.work_tree.clone();
    let repo = ctx.open()?;
    let current = repo.get_current_branch_name()?;

    let parent_name = match parent {
        StackParent::Named(name) => name,
        StackParent::Current => match current.clone() {
            Some(c) if c != oak_core::DEFAULT_BRANCH => c,
            _ => {
                return Err(OakError::InvalidArgument(
                    "--stack stacks the new branch on the current branch, but you're not on a \
                     feature branch; use `oak switch -c` for a branch off main"
                        .to_string(),
                ))
            }
        },
    };

    if repo.get_branch(&parent_name)?.is_none() {
        return Err(OakError::InvalidArgument(format!(
            "can't stack on '{parent_name}': no such local branch. Run `oak switch \
             {parent_name}` once to fetch it, then come back and stack on it"
        )));
    }
    let branch_name = match name {
        Some(name) => {
            super::branch::validate_branch_name(name)?;
            if repo.get_branch(name)?.is_some() {
                return Err(OakError::BranchAlreadyExists(name.to_string()));
            }
            name.to_string()
        }
        None => super::merge::next_open_personal_branch_name(repo.as_ref())?,
    };
    oak_core::validate_stack_parent(&branch_name, Some(&parent_name), |n| {
        local_stack_node(repo.as_ref(), n)
    })
    .map_err(|e| OakError::InvalidArgument(format!("can't stack '{branch_name}': {e}")))?;

    let stacking_on_current = current.as_deref() == Some(parent_name.as_str());
    let parent_head = repo.get_branch_head(&parent_name)?;
    let seed_head = if stacking_on_current {
        // The worktree is the parent's state (plus any edits, which carry
        // over exactly like `oak switch -c`). HEAD is the parent's head.
        repo.get_head()?.or(parent_head)
    } else {
        parent_head
    };
    let Some(seed_head) = seed_head else {
        return Err(OakError::InvalidArgument(format!(
            "'{parent_name}' has no commits yet; commit on it before stacking a branch on it"
        )));
    };

    let branch = oak_core::Branch::new(branch_name.clone(), None, Some(parent_name.clone()));
    let mut journal = None;
    if !stacking_on_current {
        // Moving to another branch's snapshot: carrying dirty edits across an
        // unrelated tree would misattribute them, so require a clean tree
        // unless the caller asked to discard.
        let holds = if policy == WorktreePolicy::Discard {
            WorktreeHolds::Unknown
        } else if worktree_is_clean_without_storing_blobs(repo.as_ref(), &root)? {
            WorktreeHolds::after_clean_guard(repo.as_ref())?
        } else {
            return Err(OakError::UncommittedChanges);
        };
        let seed_root = root_for_head(repo.as_ref(), Some(&seed_head))?;
        let plan = WorkingDirMove::plan(repo.as_ref(), &holds, &seed_root)?;
        plan.require_blobs(repo.as_ref())?;
        journal = Some(journal::Journal::begin(
            &ctx,
            repo.as_ref(),
            &plan,
            &branch_name,
            Some(seed_head.clone()),
            Some(branch.clone()),
        )?);
        plan.apply(&lock, &root, repo.as_ref())?.finish();
    } else if policy == WorktreePolicy::Discard {
        let seed_root = root_for_head(repo.as_ref(), Some(&seed_head))?;
        let plan = WorkingDirMove::plan(repo.as_ref(), &WorktreeHolds::Unknown, &seed_root)?;
        journal = Some(journal::Journal::begin(
            &ctx,
            repo.as_ref(),
            &plan,
            &branch_name,
            Some(seed_head.clone()),
            Some(branch.clone()),
        )?);
        plan.apply(&lock, &root, repo.as_ref())?.finish();
    }

    if let Some(journal) = journal {
        journal.finish(&ctx, repo.as_ref())?;
    } else {
        repo.store_branch(&branch)?;
        repo.set_branch_head(&branch_name, &seed_head)?;
        repo.set_head(&seed_head)?;
        repo.set_current_branch(&branch_name)?;
    }

    let mut chain = stack_chain(repo.as_ref(), &branch_name);
    chain.reverse();
    output::success(&format!(
        "Created branch '{branch_name}' stacked on '{parent_name}' ({})",
        chain.join(" <- ")
    ));
    Ok(())
}

/// Observe `main` for branch creation: the remote is always asked for its
/// head ([`ObservePolicy::MustVerify`]), so a branch "from latest main" never
/// starts from a `main` the remote has already moved past. When the remote
/// can't be reached the branch starts from local `main`, and the warning (and
/// the receipt's `base_head_source`) says exactly that.
fn observe_main_for_branch_creation(
    ctx: &crate::resolve::RepoContext,
) -> Result<RemoteHeadObservation> {
    let observation = match &ctx.backend {
        Backend::Sqlite => {
            let sqlite_repo = SqliteRepository::open(&ctx.db_path()?)?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            // Branch creation only needs main's head; the first merge-base
            // caller (sync/pull/push re-parent) links the full ancestry.
            rt.block_on(remote_head::observe(
                &sqlite_repo,
                ObservePolicy::MustVerify,
                ObserveOptions::default(),
            ))?
        }
        Backend::Git { .. } => {
            let repo = ctx.open()?;
            let observation = remote_head::on_record(repo.as_ref())?;
            if repo
                .get_metadata(MetadataKey::RemoteUrl)?
                .is_some_and(|remote| !remote.trim().is_empty())
            {
                output::warning(
                    "Remote refresh for git-backed repos is unsupported; using local main",
                );
            }
            observation
        }
    };
    if let RemoteHeadSource::Unverified { .. } = &observation.source {
        output::warning(&format!(
            "main {}; branching from local main{}",
            observation.describe(),
            observation
                .head
                .as_ref()
                .map(|head| format!(" ({})", head.short()))
                .unwrap_or_default()
        ));
    }
    Ok(observation)
}

/// The root tree of `head`'s snapshot; the empty tree for no head.
fn root_for_head(repo: &dyn Repository, head: Option<&Hash>) -> Result<Hash> {
    let Some(head) = head else {
        return Ok(Tree::empty_hash());
    };
    Ok(repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?
        .manifest_hash)
}

fn is_interactive() -> bool {
    output::can_prompt()
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
        options.push(format!("Stack on the current branch ('{c}')"));
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
        ParentKind::Current => match current.as_deref() {
            Some(c) if c != oak_core::DEFAULT_BRANCH => create_stacked(
                path,
                Some(name),
                StackParent::Current,
                WorktreePolicy::Carry,
            ),
            _ => super::branch::new_branch(path, name, None, None, None),
        },
        ParentKind::ServerMain => {
            // Fetch main's head + manifest + missing blobs into local
            // storage, then create the branch parented onto `main` and
            // seeded at that head — `branch::new_branch` will rewrite the
            // working tree to match.
            let db_path = ctx.db_path()?;
            let sqlite_repo = SqliteRepository::open(&db_path)?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            let head = rt
                .block_on(super::sync::fetch_parent_from_server(
                    &sqlite_repo,
                    "main",
                    super::sync::AncestryNeed::HeadOnly,
                ))?
                .ok_or_else(|| OakError::Server("Server has no commits on main yet".to_string()))?;
            drop(sqlite_repo);
            super::branch::new_branch(path, name, None, Some(head.as_str()), Some("main"))
        }
    }
}

/// What a working-tree move may assume is on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorktreeHolds {
    /// The snapshot with this root tree: the dirty guard passed against it.
    Snapshot(Hash),
    /// Unknown: the guard was skipped (`--clean`), or the snapshot it
    /// compared against isn't stored locally.
    Unknown,
}

impl WorktreeHolds {
    /// What a working tree that just passed
    /// [`worktree_is_clean_without_storing_blobs`] holds.
    pub(crate) fn after_clean_guard(repo: &dyn Repository) -> Result<Self> {
        Ok(match super::commit::clean_worktree_root(repo)? {
            Some(root) => Self::Snapshot(root),
            None => Self::Unknown,
        })
    }
}

/// Moving the working tree to another snapshot (`oak switch`, `oak switch
/// -c`, in-place `oak split`, `oak branch --from`), planned before any ref
/// moves.
///
/// When the tree is known to hold a snapshot, the move writes only the files
/// that differ from it and removes the ones the target dropped. The two
/// snapshots are compared by tree diff, so directories they share are never
/// read, and anything the dirty guard doesn't see (empty directories,
/// sockets) is left alone. Otherwise the move rewrites the whole tree from
/// the flattened target and deletes everything else, O(files).
///
/// Either way, files whose content the server withheld or declared lost are
/// skipped with a warning, as [`update_working_dir`] does.
pub(crate) struct WorkingDirMove {
    plan: MovePlan,
}

enum MovePlan {
    Diff(WorktreeDiff),
    Full(Manifest),
}

impl WorkingDirMove {
    /// Plan the move to the snapshot whose root tree is `to`.
    pub(crate) fn plan(repo: &dyn Repository, holds: &WorktreeHolds, to: &Hash) -> Result<Self> {
        let WorktreeHolds::Snapshot(from) = holds else {
            return Self::rewrite_everything(repo, to);
        };
        let mut fetch = |hash: &Hash| {
            if hash == &Tree::empty_hash() {
                return Ok(Tree::empty());
            }
            repo.get_tree(hash)?
                .ok_or_else(|| OakError::ManifestNotFound(hash.to_string()))
        };
        match WorktreeDiff::between(from, to, Removed::Delete, &mut fetch) {
            Ok(diff) => Ok(Self {
                plan: MovePlan::Diff(diff),
            }),
            // A tree on a path the snapshots disagree on is missing locally —
            // say the current snapshot's, under a directory the target drops,
            // so the diff can't list the files to remove. Rewriting the whole
            // tree needs only the target's data, as it always has.
            Err(OakError::ManifestNotFound(_)) => Self::rewrite_everything(repo, to),
            Err(error) => Err(error),
        }
    }

    fn rewrite_everything(repo: &dyn Repository, to: &Hash) -> Result<Self> {
        let manifest = if to == &Tree::empty_hash() {
            Manifest::empty()
        } else {
            repo.get_manifest(to)?
                .ok_or_else(|| OakError::ManifestNotFound(to.to_string()))?
        };
        Ok(Self {
            plan: MovePlan::Full(manifest),
        })
    }

    /// The files the move writes: those that differ, or every file of a
    /// whole-tree rewrite.
    pub(crate) fn files(&self) -> &Manifest {
        match &self.plan {
            MovePlan::Diff(diff) => diff.target(),
            MovePlan::Full(manifest) => manifest,
        }
    }

    /// Whether this is a whole-tree rewrite, which overwrites every file
    /// and removes the untracked ones: right only for `--clean` or a tree
    /// proved clean.
    pub(crate) fn rewrites_whole_tree(&self) -> bool {
        matches!(self.plan, MovePlan::Full(_))
    }

    /// Refuse, writing nothing, when the move would overwrite uncommitted
    /// work (see [`crate::worktree_update::classify`]). A whole-tree rewrite
    /// is planned only for `--clean` or a tree a clean guard passed, and
    /// overwrites files by definition — but not a file/directory
    /// obstruction, which it could not write through: that refuses here,
    /// before the switch journal exists.
    pub(crate) fn preflight(&self, root: &Path, repo: &dyn Repository) -> Result<()> {
        match &self.plan {
            MovePlan::Diff(diff) => diff.preflight(root, repo),
            MovePlan::Full(manifest) => {
                crate::worktree_update::refuse_obstructions(repo, root, manifest)
            }
        }
    }

    /// Fail where [`Self::apply`] would stop on a missing blob, without
    /// touching the working tree. For callers that move refs first.
    pub(crate) fn require_blobs(&self, repo: &dyn Repository) -> Result<()> {
        require_working_dir_blobs(repo, self.files())
    }

    /// Returns the update's record (see [`WorktreeDiff::apply`]): finish it
    /// after the refs move. Inert inside a switch, whose journal covers it.
    pub(crate) fn apply(
        &self,
        lock: &WorkdirLock,
        root: &Path,
        repo: &dyn Repository,
    ) -> Result<crate::worktree_update::WorktreeUpdate> {
        match &self.plan {
            MovePlan::Full(manifest) => {
                let update = record_whole_tree_move(lock, root, repo, manifest)?;
                match update_working_dir(lock, root, repo, manifest) {
                    Ok(()) => Ok(update),
                    Err(error) => {
                        update.abandon(lock, root, repo);
                        Err(error)
                    }
                }
            }
            MovePlan::Diff(diff) => {
                // Skipping withheld files turns off the materializer's own
                // up-front blob check, so check here: a missing blob must not
                // leave the move half-applied.
                self.require_blobs(repo)?;
                let mut update = crate::worktree_update::WorktreeUpdate::none();
                apply_skipping_withheld(repo, diff.target(), |missing_blobs, known_lost| {
                    let (report, recorded) =
                        diff.apply_with(lock, root, repo, missing_blobs, Some(known_lost))?;
                    update = recorded;
                    Ok(report)
                })?;
                Ok(update)
            }
        }
    }
}

/// Record a whole-tree rewrite that runs before its refs move, when nothing
/// says what the tree holds: capture what is on disk (storing content not
/// yet stored, as the switch journal does), so a rewrite that is killed can
/// be rolled back to exactly that. O(files), like the rewrite itself.
fn record_whole_tree_move(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    target: &Manifest,
) -> Result<crate::worktree_update::WorktreeUpdate> {
    if crate::worktree_update::covered(lock) {
        return Ok(crate::worktree_update::WorktreeUpdate::none());
    }
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(MetadataKey::SparsePaths)?.as_deref(),
    );
    let in_cone = |path: &str| cone.as_ref().is_none_or(|c| c.covers(path));
    let ignore = oak_core::IgnorePatterns::new(root)?;
    let on_disk = super::commit::scan_working_dir_for_capture(root, repo, &ignore, &in_cone)?;
    let written: std::collections::HashSet<&str> =
        target.entries.iter().map(|e| e.path.as_str()).collect();
    let (replaced, dropped): (Vec<_>, Vec<_>) = on_disk
        .into_iter()
        .partition(|entry| written.contains(entry.path.as_str()));
    crate::worktree_update::begin(
        lock,
        repo,
        root,
        crate::worktree_update::Recovery::Rollback,
        Some(crate::worktree_update::Plan {
            target,
            dropped: &dropped,
            replaced: &replaced,
            sparse_paths_before: None,
            capture_unknown: true,
            refs_after: None,
            replaced_is_disk: true,
            // A whole-tree rewrite is planned only where the user asked to
            // discard (`--clean`) or a clean guard passed: see
            // `WorkingDirMove::plan`.
            on_overlap: crate::worktree_update::OnOverlap::Overwrite,
        }),
    )
}

/// Fail where [`update_working_dir`] would stop on a missing blob, without
/// touching the working tree: every in-cone file of `manifest` that isn't
/// withheld (restricted or declared lost) needs its blob stored locally. For
/// callers that move refs before the working tree, so a missing blob stops
/// them before anything moves.
fn require_working_dir_blobs(repo: &dyn Repository, manifest: &oak_core::Manifest) -> Result<()> {
    let withheld: std::collections::HashSet<String> =
        super::restricted::restricted_paths_in_manifest(repo, manifest)
            .into_iter()
            .chain(super::known_loss::known_lost_paths_in_manifest(
                repo, manifest,
            ))
            .collect();
    let written = Manifest {
        hash: manifest.hash.clone(),
        entries: manifest
            .entries
            .iter()
            .filter(|entry| !withheld.contains(&entry.path))
            .cloned()
            .collect(),
    };
    crate::materialize::require_blobs_for_apply(repo, &written)
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
    apply_skipping_withheld(repo, manifest, |missing_blobs, known_lost| {
        crate::materialize::apply_manifest(
            lock,
            root,
            repo,
            manifest,
            crate::materialize::ApplyOpts {
                missing_blobs,
                remove_missing_paths: Some(known_lost),
                ..crate::materialize::ApplyOpts::default()
            },
        )
    })
}

/// Write `files` with `apply`, skipping the withheld ones. Files the server
/// withheld under path-based permissions can't materialize — skip exactly
/// those with an access note, and likewise files whose content was declared
/// lost (which `apply` also removes from disk: they're passed as its second
/// argument). Any other missing blob still fails loudly (`apply` gets
/// `MissingBlobs::Error` when nothing is withheld), and in skip mode, a skip
/// of anything else is re-raised as the error it would have been.
pub(crate) fn apply_skipping_withheld(
    repo: &dyn Repository,
    files: &Manifest,
    apply: impl FnOnce(MissingBlobs, &std::collections::HashSet<String>) -> Result<ApplyReport>,
) -> Result<()> {
    let restricted: std::collections::HashSet<String> =
        super::restricted::restricted_paths_in_manifest(repo, files)
            .into_iter()
            .collect();
    let known_lost: std::collections::HashSet<String> =
        super::known_loss::known_lost_paths_in_manifest(repo, files)
            .into_iter()
            .collect();
    let report = apply(
        if restricted.is_empty() && known_lost.is_empty() {
            MissingBlobs::Error
        } else {
            MissingBlobs::Skip
        },
        &known_lost,
    )?;

    let (restricted_skips, remaining_skips): (Vec<&String>, Vec<&String>) = report
        .skipped
        .iter()
        .partition(|p| restricted.contains(p.as_str()));
    let (known_lost_skips, other_skips): (Vec<&String>, Vec<&String>) = remaining_skips
        .into_iter()
        .partition(|p| known_lost.contains(p.as_str()));
    if let Some(first) = other_skips.first() {
        return Err(OakError::BlobNotFound(format!(
            "blob content for '{first}' is missing locally and could not be materialized"
        )));
    }
    if !restricted_skips.is_empty() {
        output::warning(&super::restricted::withheld_summary(restricted_skips.len()));
        for p in &restricted_skips {
            output::warning(&format!("  - {p}"));
        }
    }
    if !known_lost_skips.is_empty() {
        output::warning(&super::known_loss::warning(known_lost_skips.len()));
        for path in &known_lost_skips {
            output::warning(&format!("  - {path}"));
        }
    }
    Ok(())
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
