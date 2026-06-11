use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use oak_core::{
    three_way_merge_manifests, BranchStatus, Commit, FileChange, Hash, IgnorePatterns, Manifest,
    ManifestEntry, MergeConflict, MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use serde::Deserialize;

use crate::materialize::{apply_manifest, ApplyOpts, DeleteScope};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Merge the current branch into its parent branch.
///
/// When the parent is `main` the merge can't happen locally — `main` only
/// exists on the server, and the canonical merge is a squash-merge whose
/// commit message is the branch's description (see CLAUDE.md). In that
/// case we call the server's
/// `POST /api/:owner/:name/branches/:brname/merge` endpoint and
/// mirror the resulting closed state into the local repo.
///
/// The remote comes from the repo's stored `RemoteUrl`; when that host
/// answers with a redirect to a trusted Oak host, the stored remote is
/// updated and the merge retried once — see `follow_remote_move`. The
/// first attempt fails on its first request (the pre-merge push's HEAD
/// GET), before anything is mutated, so the retry re-runs the whole flow
/// safely.
pub async fn run(path: &Path, continue_merge: bool, abort_merge: bool) -> Result<()> {
    match run_once(path, continue_merge, abort_merge).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            let old = super::stored_remote_url(path)?.unwrap_or_default();
            super::follow_remote_move(path, &old, &origin)?;
            run_once(path, continue_merge, abort_merge).await
        }
        result => result,
    }
}

async fn run_once(path: &Path, continue_merge: bool, abort_merge: bool) -> Result<()> {
    if continue_merge {
        return merge_continue(path);
    }
    if abort_merge {
        return merge_abort(path);
    }

    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;

    // Check if a merge is already in progress. The merge state files live in
    // the backend's sidecar dir (`.oak/` for sqlite, `.git/oak/` for git).
    if ctx.oak_dir.join("MERGE_HEAD").exists() {
        return Err(OakError::MergeInProgress);
    }

    // Refuse to merge over uncommitted work. Both merge paths end in a
    // whole-tree reset (to the merged manifest locally, or to main's new
    // head after a server squash-merge), which would silently wipe any
    // uncommitted edits.
    let (changes, _, _) = super::commit::get_status(path)?;
    if !changes.is_empty() {
        return Err(OakError::MergeFailed(
            "working tree has uncommitted changes — run `oak commit` to keep them \
             (or `oak reset` to discard them) before merging"
                .to_string(),
        ));
    }

    let repo = ctx.open()?;

    // Get current branch name
    let branch_name = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;

    // Get the branch and verify it has a parent
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;

    let parent_name = branch.parent_branch.ok_or_else(|| {
        OakError::BranchNotFound(format!(
            "branch '{branch_name}' has no parent to merge into"
        ))
    })?;

    // `main` only exists on the server. Landing a branch onto main goes
    // through the server's squash-merge endpoint, where the branch's
    // description becomes the (single) commit message.
    if parent_name == "main" {
        // Push the branch first: the server merges *its* copy of the branch,
        // so `oak commit && oak merge` without a push used to squash-merge a
        // stale server-side tip and then reset the worktree over the
        // unpushed commits. A successful push leaves the server's branch
        // head equal to the local one (the server CAS-checks the expected
        // head); any push failure aborts the merge. The git backend can't
        // push (sqlite-only), so it keeps the old behavior.
        if matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
            push_branch_for_merge(&ctx, &branch_name).await?;
        }
        return merge_to_main_via_server(&lock, repo.as_ref(), root, &branch_name).await;
    }

    // Get the current branch's head commit
    let branch_head = repo
        .get_branch_head(&branch_name)?
        .ok_or(OakError::NoCommits)?;

    let branch_commit = repo
        .get_commit(&branch_head)?
        .ok_or_else(|| OakError::CommitNotFound(branch_head.to_string()))?;

    // Get the parent branch's head commit (if any)
    let parent_head = repo.get_branch_head(&parent_name)?;

    let parent_commit = if let Some(ref ph) = parent_head {
        Some(
            repo.get_commit(ph)?
                .ok_or_else(|| OakError::CommitNotFound(ph.to_string()))?,
        )
    } else {
        None
    };

    // Get manifests
    let branch_manifest = repo
        .get_manifest(&branch_commit.manifest_hash)?
        .unwrap_or_else(Manifest::empty);

    let parent_manifest = match &parent_commit {
        Some(c) => repo
            .get_manifest(&c.manifest_hash)?
            .unwrap_or_else(Manifest::empty),
        None => Manifest::empty(),
    };

    // Find the LCA (Lowest Common Ancestor) manifest
    let base_manifest = find_lca_manifest_with_parent(
        repo.as_ref(),
        &branch_name,
        &parent_name,
        parent_head.as_ref(),
    )?;

    // Pure path-by-path merge — emits clean entries plus a list of conflicts
    // resolved below (text → marker, binary → keep branch, etc.).
    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);

    let mut merged_entries = outcome.clean_entries;
    let resolution = resolve_conflicts_with_markers(
        repo.as_ref(),
        &base_manifest,
        outcome.conflicts,
        &branch_name,
        &parent_name,
    )?;
    merged_entries.extend(resolution.entries);
    let conflict_paths = resolution.conflict_paths;
    let binary_conflict_paths = resolution.binary_conflict_paths;

    let total_conflicts = conflict_paths.len() + binary_conflict_paths.len();

    if total_conflicts > 0 {
        // Write conflicted files to working directory
        let merged_manifest = Manifest::new(merged_entries);
        write_manifest_to_workdir(&lock, root, repo.as_ref(), &merged_manifest)?;

        // Save merge state in the backend's sidecar dir.
        let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
        let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");
        fs::write(&merge_head_path, format!("{parent_name}\n{branch_name}"))?;
        fs::write(
            &merge_msg_path,
            format!("Merge branch '{branch_name}' into '{parent_name}'"),
        )?;

        output::warning(&format!(
            "Merge conflict: {total_conflicts} file(s) need manual resolution"
        ));
        for p in &conflict_paths {
            output::info(&format!("  CONFLICT (content): {p}"));
        }
        for p in &binary_conflict_paths {
            output::info(&format!("  CONFLICT (binary): {p} - kept branch version"));
        }
        output::info("");
        output::info("Fix the conflicts and then run 'oak merge --continue'");
        output::info("To abort the merge, run 'oak merge --abort'");

        return Err(OakError::MergeConflict(total_conflicts));
    }

    // No conflicts - complete the merge
    let merged_manifest = Manifest::new(merged_entries);
    complete_merge(
        &lock,
        repo.as_ref(),
        root,
        &branch_name,
        &parent_name,
        parent_head,
        Some(branch_head.clone()),
        &merged_manifest,
    )?;

    Ok(())
}

/// Resolution of the per-path conflicts a manifest-level 3-way merge emitted,
/// using the CLI's policy: text conflicts get diffy conflict markers (and only
/// count as conflicts when markers actually survive — non-overlapping edits
/// merge clean), binary conflicts keep the branch version, modify/delete keeps
/// the surviving side. Shared by `oak merge` and `oak pull`'s sync phase so
/// the two can't drift.
pub(crate) struct ConflictResolution {
    pub entries: Vec<ManifestEntry>,
    pub conflict_paths: Vec<String>,
    pub binary_conflict_paths: Vec<String>,
}

pub(crate) fn resolve_conflicts_with_markers(
    repo: &dyn Repository,
    base_manifest: &Manifest,
    conflicts: Vec<MergeConflict>,
    branch_name: &str,
    parent_name: &str,
) -> Result<ConflictResolution> {
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut conflict_paths: Vec<String> = Vec::new();
    let mut binary_conflict_paths: Vec<String> = Vec::new();

    for MergeConflict {
        path,
        branch_entry,
        parent_entry,
    } in conflicts
    {
        match (branch_entry, parent_entry) {
            (Some(be), Some(pe)) => {
                // Both modified differently
                let bb = repo
                    .get_blob(&be.blob_hash)?
                    .ok_or_else(|| OakError::BlobNotFound(be.blob_hash.to_string()))?;
                let pb = repo
                    .get_blob(&pe.blob_hash)?
                    .ok_or_else(|| OakError::BlobNotFound(pe.blob_hash.to_string()))?;
                // The base (LCA) version is what makes diffy's 3-way merge
                // pinpoint just the diverging hunks. If the path didn't
                // exist in the LCA — added on both sides with different
                // content — there's no base to diff against, so feed an
                // empty string and let diffy treat the whole file as the
                // conflict.
                let base_text = base_manifest
                    .entries
                    .iter()
                    .find(|e| e.path == path)
                    .and_then(|e| repo.get_blob(&e.blob_hash).ok().flatten())
                    .and_then(|b| String::from_utf8(b.content).ok())
                    .unwrap_or_default();

                let branch_text = String::from_utf8(bb.content.clone());
                let parent_text = String::from_utf8(pb.content.clone());

                match (branch_text, parent_text) {
                    (Ok(bt), Ok(pt)) => {
                        let merged =
                            create_conflict_content(&base_text, &bt, &pt, branch_name, parent_name);
                        // Both sides editing the same file only means the
                        // manifest hashes diverge — NOT that the edits collide.
                        // diffy reconciles non-overlapping hunks cleanly (no
                        // markers); only flag a conflict when markers survive.
                        let had_conflict = content_has_conflict_markers(&merged);
                        let blob_hash = repo.put_blob(merged.into_bytes())?;
                        entries.push(ManifestEntry {
                            path: path.clone(),
                            blob_hash,
                            mode: be.mode,
                        });
                        if had_conflict {
                            conflict_paths.push(path);
                        }
                    }
                    _ => {
                        // Binary: keep branch version
                        entries.push(be);
                        binary_conflict_paths.push(path);
                    }
                }
            }
            (Some(be), None) => {
                // Parent deleted, branch modified
                entries.push(be);
                conflict_paths.push(path);
            }
            (None, Some(pe)) => {
                // Branch deleted, parent modified
                entries.push(pe);
                conflict_paths.push(path);
            }
            (None, None) => {
                // Both deleted — nothing to add (shouldn't appear in conflicts)
            }
        }
    }

    Ok(ConflictResolution {
        entries,
        conflict_paths,
        binary_conflict_paths,
    })
}

/// Push the current branch before a server-side squash-merge, so the server
/// merges the tip we actually have. Pushing also guarantees the server's
/// branch head equals the local one on return: the push protocol CAS-checks
/// the expected branch head and errors on divergence, and that error aborts
/// the merge.
async fn push_branch_for_merge(ctx: &crate::resolve::RepoContext, branch_name: &str) -> Result<()> {
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server(
            "Repository has no remote configured. Run `oak push` to link it to a server."
                .to_string(),
        )
    })?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        OakError::Server(
            "Repository isn't linked to an organization yet. Run `oak push` first.".to_string(),
        )
    })?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing repo name; re-clone with `oak clone <org>/<repo>`."
                .to_string(),
        )
    })?;
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    super::push::push_async(
        &repo,
        &ctx.work_tree,
        &remote,
        &owner,
        &repo_name,
        Some(branch_name),
        false,
        api_key.as_deref(),
    )
    .await
}

/// 3-way line merge of `branch_text` and `parent_text` against the common
/// ancestor `base_text`. Non-conflicting hunks come through clean; only the
/// diverging regions get wrapped in standard conflict markers labelled with
/// the branch names.
///
/// Returns the merged content whether or not it conflicted. Callers must NOT
/// assume the result is conflicted just because both sides touched the file:
/// diverging manifest hashes only mean both edited it, and diffy reconciles
/// non-overlapping edits into clean, marker-free output. Use
/// [`content_has_conflict_markers`] on the result to decide whether the path
/// is actually a conflict.
pub(crate) fn create_conflict_content(
    base_text: &str,
    branch_text: &str,
    parent_text: &str,
    branch_name: &str,
    parent_name: &str,
) -> String {
    let merged = match diffy::merge(base_text, branch_text, parent_text) {
        Ok(clean) => clean,
        Err(conflicted) => conflicted,
    };
    // diffy hardcodes the marker labels "ours" / "theirs" (and "original"
    // for the diff3 conflict style; we use the default Merge style which
    // omits it). Rewrite to the actual branch names so the user can tell
    // which side is which.
    merged
        .replace(
            &format!("{OURS_MARKER} ours"),
            &format!("{OURS_MARKER} {branch_name}"),
        )
        .replace(
            &format!("{THEIRS_MARKER} theirs"),
            &format!("{THEIRS_MARKER} {parent_name}"),
        )
}

// Conflict markers — kept as concat!() of two halves so that this very file
// (which contains the find_conflict_markers scanner below) doesn't trip its
// own naive substring check. The compiler still folds these to constants.
const OURS_MARKER: &str = concat!("<<<<", "<<<");
const THEIRS_MARKER: &str = concat!(">>>>", ">>>");

/// Find the LCA manifest between two branches by walking commit parent chains.
/// When the parent branch is `main` (which has no local branch row), pass its
/// head hash explicitly to ensure proper ancestor collection.
pub(crate) fn find_lca_manifest_with_parent(
    repo: &dyn Repository,
    branch_a: &str,
    branch_b: &str,
    branch_b_head: Option<&Hash>,
) -> Result<Manifest> {
    // Collect ancestor hashes for both branches
    let a_ancestors = collect_ancestors(repo, branch_a)?;
    let b_ancestors = if let Some(head) = branch_b_head {
        collect_ancestors_from_head(repo, head)?
    } else {
        collect_ancestors(repo, branch_b)?
    };
    let b_ancestor_set: HashSet<String> = b_ancestors.iter().map(|h| h.to_string()).collect();

    // The first ancestor of branch_a that also appears in branch_b's history is the LCA
    for hash in &a_ancestors {
        if b_ancestor_set.contains(&hash.to_string()) {
            if let Some(commit) = repo.get_commit(hash)? {
                if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
                    return Ok(manifest);
                }
            }
        }
    }

    // Fallback: try parent of first commit on branch_a
    let a_commits = repo.get_commits_for_branch(branch_a)?;
    if let Some(first_commit) = a_commits.first() {
        if let Some(ref parent_hash) = first_commit.parent_hash {
            if let Some(parent_commit) = repo.get_commit(parent_hash)? {
                if let Some(manifest) = repo.get_manifest(&parent_commit.manifest_hash)? {
                    return Ok(manifest);
                }
            }
        }
    }

    Ok(Manifest::empty())
}

/// Collect ancestor commit hashes starting from a specific head hash using BFS.
/// Follows both parent_hash and merge_parent_hash to correctly traverse the DAG.
fn collect_ancestors_from_head(repo: &dyn Repository, head: &Hash) -> Result<Vec<Hash>> {
    let mut ancestors = Vec::new();
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    queue.push_back(head.clone());

    while let Some(hash) = queue.pop_front() {
        if !visited.insert(hash.to_string()) {
            continue;
        }
        ancestors.push(hash.clone());
        if let Some(commit) = repo.get_commit(&hash)? {
            if let Some(parent) = commit.parent_hash {
                queue.push_back(parent);
            }
            if let Some(merge_parent) = commit.merge_parent_hash {
                queue.push_back(merge_parent);
            }
        }
    }

    Ok(ancestors)
}

/// Collect ancestor commit hashes for a branch using BFS.
/// Follows both parent_hash and merge_parent_hash to correctly
/// traverse the full DAG including merge commits.
fn collect_ancestors(repo: &dyn Repository, branch_name: &str) -> Result<Vec<Hash>> {
    let head = repo.get_branch_head(branch_name)?;
    if let Some(h) = head {
        collect_ancestors_from_head(repo, &h)
    } else {
        Ok(Vec::new())
    }
}

/// Write manifest files to the working directory without deleting anything —
/// the conflict write-out. Untracked files and paths outside the manifest are
/// left alone (the user resolves conflicts in place and then `--continue`s).
/// Routed through the shared materializer, which also refreshes the stat
/// cache for every file written so a `oak status` run between writing the
/// conflicted tree and `--continue` doesn't trust a stale row.
pub(crate) fn write_manifest_to_workdir(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    manifest: &Manifest,
) -> Result<()> {
    apply_manifest(
        lock,
        root,
        repo,
        manifest,
        ApplyOpts {
            delete: DeleteScope::Nothing,
            clean_empty_dirs: false,
            ..ApplyOpts::default()
        },
    )
    .map(|_| ())
}

/// Complete the merge: create commit, update heads, close branch, switch
#[allow(clippy::too_many_arguments)]
fn complete_merge(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
    parent_name: &str,
    parent_head: Option<Hash>,
    branch_head: Option<Hash>,
    merged_manifest: &Manifest,
) -> Result<()> {
    let parent_manifest = if let Some(ref ph) = parent_head {
        let pc = repo
            .get_commit(ph)?
            .ok_or_else(|| OakError::CommitNotFound(ph.to_string()))?;
        repo.get_manifest(&pc.manifest_hash)?
            .unwrap_or_else(Manifest::empty)
    } else {
        Manifest::empty()
    };

    let changes = parent_manifest.diff(merged_manifest);
    let file_changes: Vec<FileChange> = changes
        .iter()
        .map(|c| FileChange {
            path: c.path.clone(),
            change_type: c.change_type,
            old_blob_hash: c.old_blob_hash.clone(),
            new_blob_hash: c.new_blob_hash.clone(),
            old_path: c.old_path.clone(),
            old_mode: None,
            new_mode: None,
        })
        .collect();

    let author = std::env::var("OAK_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());

    // Store manifest first so the commit references the canonical hash from this
    // backend (BLAKE3 for sqlite, tree OID for git). The git backend rebuilds the
    // tree from the entries here; the in-memory `merged_manifest.hash` (BLAKE3)
    // would otherwise be wrong on git.
    let manifest_hash = repo.put_manifest(merged_manifest.entries.clone())?;

    // Local merge commits don't carry messages: a real message only gets
    // attached when the server squash-merges a feature branch onto main,
    // and that path uses the source branch's description.
    let commit_hash = repo.put_commit(
        parent_name.to_string(),
        parent_head,
        branch_head,
        manifest_hash,
        author,
        None,
        chrono::Utc::now(),
        file_changes,
    )?;

    repo.set_branch_head(parent_name, &commit_hash)?;
    repo.set_head(&commit_hash)?;
    // The merged-from branch's work now lives on the parent — drop its row
    // so it stops showing up in `oak switch` / `oak branch list`. Backends
    // without delete support (git sidecar) fall back to closing it; both
    // calls are metadata-only and tolerated as best-effort.
    let _ = repo.update_branch_status(branch_name, BranchStatus::Closed);
    repo.set_current_branch(parent_name)?;
    let removed = repo.delete_branch(branch_name).is_ok();

    // Update working directory to merged state
    crate::commands::reset::reset_to_manifest(lock, root, repo, merged_manifest)?;

    output::success(&format!(
        "Merged '{}{}{}' into '{}{}{}'",
        output::colors::BOLD,
        branch_name,
        output::colors::RESET,
        output::colors::BOLD,
        parent_name,
        output::colors::RESET,
    ));
    output::item(&format!(
        "commit {}{}{}",
        output::colors::CYAN,
        commit_hash.short(),
        output::colors::RESET,
    ));
    output::item(&format!(
        "branch '{}' {}, switched to '{}{}{}'",
        branch_name,
        if removed { "removed" } else { "closed" },
        output::colors::BOLD,
        parent_name,
        output::colors::RESET,
    ));

    Ok(())
}

/// Continue a merge after conflicts have been resolved
fn merge_continue(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
    let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");

    if !merge_head_path.exists() {
        return Err(OakError::NoMergeInProgress);
    }

    // Read merge state
    let merge_head_content = fs::read_to_string(&merge_head_path)?;
    let mut lines = merge_head_content.lines();
    let parent_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();
    let branch_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();

    // Scan working directory for remaining conflict markers
    let ignore = IgnorePatterns::new(root)?;
    let conflicted = find_conflict_markers(root, root, &ignore)?;

    if !conflicted.is_empty() {
        output::error(&format!(
            "{} file(s) still have conflict markers:",
            conflicted.len()
        ));
        for p in &conflicted {
            output::info(&format!("  {p}"));
        }
        output::info("Edit these files to resolve conflicts, then run 'oak merge --continue'");
        return Err(OakError::MergeConflict(conflicted.len()));
    }

    // No conflicts remain - build manifest from working directory and complete
    // merge. Use the no-cache scan so the merge commit reflects on-disk content
    // and never echoes a stale path-keyed stat-cache row from another branch
    // (the foreign-blob bug); this runs once per conflicted merge.
    let entries =
        crate::commands::commit::scan_working_dir_no_cache(root, root, repo.as_ref(), &ignore)?;
    let merged_manifest = Manifest::new(entries);

    let parent_head = repo.get_branch_head(&parent_name)?;
    let branch_head = repo.get_branch_head(&branch_name)?;

    complete_merge(
        &lock,
        repo.as_ref(),
        root,
        &branch_name,
        &parent_name,
        parent_head,
        branch_head,
        &merged_manifest,
    )?;

    // Clean up merge state files
    fs::remove_file(&merge_head_path)?;
    fs::remove_file(&merge_msg_path)?;

    Ok(())
}

/// Abort a merge in progress
fn merge_abort(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let repo = ctx.open()?;

    let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
    let merge_msg_path = ctx.oak_dir.join("MERGE_MSG");

    if !merge_head_path.exists() {
        return Err(OakError::NoMergeInProgress);
    }

    // Read merge state to get the current branch name
    let merge_head_content = fs::read_to_string(&merge_head_path)?;
    let mut lines = merge_head_content.lines();
    let _parent_name = lines.next();
    let branch_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();

    // Reset working directory to the current branch's HEAD
    let head_hash = repo.get_branch_head(&branch_name)?;
    if let Some(ref h) = head_hash {
        let commit = repo
            .get_commit(h)?
            .ok_or_else(|| OakError::CommitNotFound(h.to_string()))?;
        let manifest = repo
            .get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(Manifest::empty);
        crate::commands::reset::reset_to_manifest(&lock, root, repo.as_ref(), &manifest)?;
    }

    // Clean up merge state files
    fs::remove_file(&merge_head_path).ok();
    fs::remove_file(&merge_msg_path).ok();

    output::success("Merge aborted, working directory restored");

    Ok(())
}

/// Recursively scan for files containing conflict markers
pub(crate) fn find_conflict_markers(
    dir: &Path,
    root: &Path,
    ignore: &IgnorePatterns,
) -> Result<Vec<String>> {
    let mut conflicted = Vec::new();

    if !dir.exists() {
        return Ok(conflicted);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();

        if ignore.is_ignored(relative, path.is_dir()) {
            continue;
        }

        if path.is_dir() {
            conflicted.extend(find_conflict_markers(&path, root, ignore)?);
        } else if path.is_file() {
            if let Ok(content) = fs::read_to_string(&path) {
                if content_has_conflict_markers(&content) {
                    conflicted.push(relative.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(conflicted)
}

/// True if `content` still contains unresolved 3-way conflict markers.
///
/// Real markers are written at column 0 (see `create_conflict_content`), so we
/// anchor the check to line starts. A plain `contains` would false-positive on
/// source that merely mentions the marker strings in doc comments, test
/// fixtures, or user-facing error messages — none of which are at column 0 —
/// and would refuse to complete an otherwise-resolved merge.
pub(crate) fn content_has_conflict_markers(content: &str) -> bool {
    let has_ours = content.lines().any(|l| l.starts_with(OURS_MARKER));
    let has_theirs = content.lines().any(|l| l.starts_with(THEIRS_MARKER));
    has_ours && has_theirs
}

#[derive(Deserialize)]
struct MergeResponse {
    message: String,
    commit_hash: String,
    // Shape of the squash commit the server just landed on `main`. Present
    // on current servers; absent (defaulted to None) when talking to an
    // older server, in which case the caller falls back to fetching main's
    // tree. See `try_ingest_merged_main`.
    #[serde(default)]
    manifest_hash: Option<String>,
    #[serde(default)]
    parent_hash: Option<String>,
    #[serde(default)]
    merge_parent_hash: Option<String>,
}

/// Squash-merge a branch onto `main` by delegating to the server. The
/// server is the only place `main` exists and is the only writer that
/// uses the branch description as the merge commit's message.
///
/// After the server merge lands, this also auto-syncs the new personal
/// branch to main's fresh HEAD: it pulls the squash commit and manifest
/// into local storage, points the new branch at that head, and resets
/// the working tree to match. Without this, the new local branch would
/// sit on the old branch tip — out of sync with main, unreachable from
/// `oak pull`, and effectively guaranteed to "conflict on the entire
/// file" on the next `oak sync` because the LCA finder couldn't locate
/// a common ancestor.
pub(crate) async fn merge_to_main_via_server(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    branch_name: &str,
) -> Result<()> {
    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server(
            "Repository has no remote configured. Run `oak push` to link it to a server."
                .to_string(),
        )
    })?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        OakError::Server(
            "Repository isn't linked to an organization yet. Run `oak push` first.".to_string(),
        )
    })?;
    let repo_name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing repo name; re-clone with `oak clone <org>/<repo>`."
                .to_string(),
        )
    })?;

    let token = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    // Server mounts repo routes directly under `/api` (no `repos/` prefix):
    // see `build_api_routes` in oak-server/src/lib.rs. The trailing-slash trim
    // keeps a `https://oak.space/` remote from producing a `//api/...` URL.
    let url = format!(
        "{remote}/api/{owner}/{repo_name}/branches/{branch_name}/merge",
        remote = remote.trim_end_matches('/'),
    );

    let client = crate::http::api_client();
    let mut req = client.post(&url);
    if let Some(t) = &token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_merge_conflict_error(branch_name, &body));
    }
    if !resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Merge failed: {}",
            crate::http::error_text(resp).await
        )));
    }

    let merged: MergeResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    output::success(&format!("Merged '{branch_name}' onto main"));
    output::detail("commit", &merged.commit_hash);
    output::detail("message", &merged.message);

    // The server closed the branch as part of the merge — mirror that
    // locally so `oak switch` matches server state.
    if let Err(e) = repo.update_branch_status(branch_name, BranchStatus::Closed) {
        output::warning(&format!(
            "Local branch state not updated (merge still succeeded on server): {e}"
        ));
    }

    // Bring the local copy of `main` up to the squash commit the server
    // just landed. The working tree we're holding is stale the moment the
    // merge returns, and storing the squash commit locally is what lets
    // the LCA finder behave correctly the next time a descendant runs
    // `oak pull`.
    //
    // Fast path: a clean merge of a branch that was up to date with main
    // produces a merged tree that is byte-for-byte the manifest this
    // branch already holds (content-addressing). When that manifest is
    // already in local storage we reconstruct main's new HEAD from the
    // hashes in the merge response — no network round-trips, no re-walking
    // main's whole tree.
    //
    // Slow path: only when the merged manifest is new to us (main moved
    // under the branch, so the 3-way merge produced a tree we've never
    // stored) do we fetch it from the server. If that fetch fails (network
    // blip, server glitch), the merge still succeeded on the server, so we
    // warn and fall back to pinning the new branch at the old branch tip;
    // `oak pull` recovers once connectivity returns.
    let new_main_head = match try_ingest_merged_main(repo, &merged)? {
        Some(h) => Some(h),
        None => match super::sync::fetch_parent_from_server(repo, "main").await {
            Ok(Some(h)) => Some(h),
            Ok(None) => None,
            Err(e) => {
                output::warning(&format!(
                    "Couldn't refresh main locally (merge succeeded on server): {e}"
                ));
                None
            }
        },
    };

    // Create a fresh personal branch parented onto main and switch to
    // it. Two cases:
    //   * Auto-sync worked → seed at main's new HEAD and reset the
    //     working tree. The new branch starts with zero local commits,
    //     in perfect sync with main.
    //   * Auto-sync failed → fall back to seeding at the just-merged
    //     branch's old tip. The working tree already matches that
    //     manifest, so we don't need to touch it.
    let new_branch_name = next_open_personal_branch_name(repo)?;
    let new_br = oak_core::Branch::new(new_branch_name.clone(), None, Some("main".to_string()));
    repo.store_branch(&new_br)?;

    let seed_head = if let Some(ref h) = new_main_head {
        Some(h.clone())
    } else {
        repo.get_branch_head(branch_name)?
    };
    if let Some(ref head) = seed_head {
        repo.set_branch_head(&new_branch_name, head)?;
        // Move HEAD onto the new branch's tip too. `set_current_branch`
        // alone leaves HEAD pointing at the just-closed feature branch's
        // commit, so the next `oak commit` would parent off the wrong
        // commit (and `oak hash` would lie). Mirror `oak switch`, which
        // always pairs the current-branch update with a `set_head`.
        repo.set_head(head)?;
    }
    repo.set_current_branch(&new_branch_name)?;

    // Reset the working tree to main's new manifest, but only when
    // auto-sync gave us a head whose commit + manifest are now in
    // local storage. The reset writes every file in the manifest and
    // deletes anything outside it, so we must skip it on the fallback
    // path — otherwise we'd blow away the user's working tree based on
    // an unknown manifest.
    if let Some(ref head) = new_main_head {
        if let Some(commit) = repo.get_commit(head)? {
            if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
                crate::commands::reset::reset_to_manifest(lock, root, repo, &manifest)?;
            }
        }
    }

    // The branch's work landed on main — drop the local row entirely (the
    // server keeps the authoritative closed record; local commits stay,
    // branch_name is a soft label). Backends without delete support keep
    // the closed row from above.
    let removed = repo.delete_branch(branch_name).is_ok();

    output::item(&format!(
        "branch '{branch_name}' {}, switched to new branch '{}{}{}'  (parented onto main)",
        if removed { "removed" } else { "closed" },
        output::colors::BOLD,
        new_branch_name,
        output::colors::RESET,
    ));

    Ok(())
}

/// Render a server-side squash-merge conflict (HTTP 409) the way the local
/// merge path renders its conflicts: name each file, then say how to fix it.
/// The server lists the conflicting paths in its error message ("… on both
/// branches: a, b"); a structured `conflict_paths` field wins when present.
/// Anything that doesn't match the server's envelope (a proxy error page,
/// an older server) falls back to the generic rendering.
fn server_merge_conflict_error(branch_name: &str, body: &str) -> OakError {
    #[derive(serde::Deserialize)]
    struct ConflictBody {
        error: String,
        #[serde(default)]
        conflict_paths: Vec<String>,
    }

    let Ok(parsed) = serde_json::from_str::<ConflictBody>(body.trim()) else {
        let body = body.trim();
        return OakError::Server(if body.is_empty() {
            "Merge failed: HTTP 409 Conflict".to_string()
        } else {
            format!("Merge failed: HTTP 409 Conflict: {body}")
        });
    };

    let paths = if parsed.conflict_paths.is_empty() {
        parsed
            .error
            .rsplit_once("both branches: ")
            .map(|(_, list)| list.split(", ").map(str::to_string).collect())
            .unwrap_or_default()
    } else {
        parsed.conflict_paths
    };
    if paths.is_empty() {
        return OakError::Server(format!("Merge failed: {}", parsed.error));
    }

    output::warning(&format!(
        "Merge conflict: {} file(s) changed on both '{branch_name}' and main",
        paths.len()
    ));
    for p in &paths {
        output::info(&format!("  CONFLICT: {p}"));
    }
    output::info("");
    output::info("Run 'oak pull' to merge main into this branch and resolve the conflicts,");
    output::info("then run 'oak merge' again.");

    OakError::MergeConflict(paths.len())
}

pub(crate) struct RemoteMergeReconcile {
    pub closed_branch: String,
    pub new_branch: String,
    pub reset_worktree: bool,
}

pub(crate) fn print_remote_merge_reconcile(reconciled: &RemoteMergeReconcile) {
    output::item(&format!(
        "branch '{}' was already merged on main; switched to new branch '{}{}{}'",
        reconciled.closed_branch,
        output::colors::BOLD,
        reconciled.new_branch,
        output::colors::RESET,
    ));
    if reconciled.reset_worktree {
        output::info("Updated working tree to latest main.");
    } else {
        output::warning("Preserved uncommitted changes on the new branch.");
    }
}

pub(crate) struct RemoteMergeReconcilePlan {
    branch_name: String,
    branch_head: Hash,
    main_head: Hash,
}

pub(crate) enum RemoteMergeReconcileScope<'a> {
    CurrentBranch,
    Pull {
        branch_open_at_start: Option<&'a str>,
    },
}

/// Mirror the local post-merge state when another client already squash-merged
/// the current branch onto `main`.
///
/// `oak fetch` / `oak status` can learn that fact after local `main` history is
/// refreshed: the squash commit on `main` carries the merged branch tip in
/// `merge_parent_hash`. When we see that ancestry locally, perform the same
/// branch transition as `oak merge`: close the old branch, create a fresh
/// branch parented onto `main`, point it at main's current head, and switch to
/// it. Clean worktrees are reset to main; dirty worktrees are left untouched so
/// uncommitted edits move onto the fresh branch instead of being discarded.
pub(crate) fn apply_remote_merge_reconcile(
    lock: &WorkdirLock,
    repo: &dyn Repository,
    root: &Path,
    working_tree_clean: bool,
    plan: RemoteMergeReconcilePlan,
) -> Result<Option<RemoteMergeReconcile>> {
    let Some(current_branch) = repo.get_current_branch_name()? else {
        return Ok(None);
    };
    if current_branch != plan.branch_name {
        return Ok(None);
    }
    if repo.get_branch_head(&plan.branch_name)? != Some(plan.branch_head.clone()) {
        return Ok(None);
    }
    if repo.get_branch_head("main")? != Some(plan.main_head.clone()) {
        return Ok(None);
    }

    let main_manifest = if working_tree_clean {
        let Some(main_commit) = repo.get_commit(&plan.main_head)? else {
            return Ok(None);
        };
        let Some(main_manifest) = repo.get_manifest(&main_commit.manifest_hash)? else {
            return Ok(None);
        };
        Some(main_manifest)
    } else {
        None
    };

    let branch_name = plan.branch_name;
    let main_head = plan.main_head;

    let new_branch_name = next_open_personal_branch_name(repo)?;
    let new_br = oak_core::Branch::new(new_branch_name.clone(), None, Some("main".to_string()));
    repo.reconcile_remote_merge_branch_state(&branch_name, &new_br, &main_head)?;
    // The merged branch's work is on main now — drop the closed tombstone
    // the reconcile transaction left behind (best-effort; backends without
    // delete support keep the closed row).
    let _ = repo.delete_branch(&branch_name);

    if let Some(main_manifest) = main_manifest {
        crate::commands::reset::reset_to_manifest(lock, root, repo, &main_manifest)?;
    }

    Ok(Some(RemoteMergeReconcile {
        closed_branch: branch_name,
        new_branch: new_branch_name,
        reset_worktree: working_tree_clean,
    }))
}

pub(crate) fn plan_remote_merge_reconcile(
    repo: &dyn Repository,
    scope: RemoteMergeReconcileScope<'_>,
) -> Result<Option<RemoteMergeReconcilePlan>> {
    let allow_closed_for_branch = match scope {
        RemoteMergeReconcileScope::CurrentBranch => None,
        RemoteMergeReconcileScope::Pull {
            branch_open_at_start,
        } => match branch_open_at_start {
            Some(branch_name) => Some(branch_name),
            None => return Ok(None),
        },
    };

    let Some(branch_name) = repo.get_current_branch_name()? else {
        return Ok(None);
    };
    if branch_name.is_empty() || branch_name == "main" {
        return Ok(None);
    }

    let Some(branch) = repo.get_branch(&branch_name)? else {
        return Ok(None);
    };
    if branch.parent_branch.as_deref() != Some("main") {
        return Ok(None);
    }
    if let Some(expected_branch) = allow_closed_for_branch {
        if branch_name != expected_branch {
            return Ok(None);
        }
    }
    // Normal reconciliation is an Open -> Closed transition. `oak pull` is the
    // exception: it may fetch server metadata that marks the current branch
    // closed before main is refreshed, so pull passes the exact branch that was
    // open when the command began. Closed branches from any other path remain a
    // no-op to avoid manual-close and switch-back rotations.
    if branch.status == BranchStatus::Closed && allow_closed_for_branch.is_none() {
        return Ok(None);
    }

    let Some(branch_head) = repo.get_branch_head(&branch_name)? else {
        return Ok(None);
    };
    if !repo.merge_child_exists(&branch_head)? {
        return Ok(None);
    }

    let Some(main_head) = repo.get_branch_head("main")? else {
        return Ok(None);
    };

    let merged_into_main = main_history_contains_merge_parent(repo, &main_head, &branch_head)?;
    if !merged_into_main {
        return Ok(None);
    }

    Ok(Some(RemoteMergeReconcilePlan {
        branch_name,
        branch_head,
        main_head,
    }))
}

fn main_history_contains_merge_parent(
    repo: &dyn Repository,
    main_head: &Hash,
    merged_branch_head: &Hash,
) -> Result<bool> {
    const MAX_MAIN_WALK: usize = 100_000;

    let mut seen = HashSet::new();
    let mut current = Some(main_head.clone());
    let mut walked = 0usize;
    while let Some(hash) = current {
        if walked >= MAX_MAIN_WALK || !seen.insert(hash.clone()) {
            break;
        }
        walked += 1;

        let Some(commit) = repo.get_commit(&hash)? else {
            break;
        };
        if commit.merge_parent_hash.as_ref() == Some(merged_branch_head) {
            return Ok(true);
        }
        // Walk the first-parent `main` chain only. `merge_parent_hash` points
        // at merged feature branches; following it would leave main history and
        // turn unrelated feature ancestry into merge proof.
        current = commit.parent_hash;
    }

    Ok(false)
}

/// Collect the tip of every branch that `main`'s first-parent history
/// records as squash-merged: each squash commit carries the merged branch's
/// tip in `merge_parent_hash`. Only main's chain is walked — sync commits on
/// feature branches also carry `merge_parent_hash` (pointing at main), and
/// trusting those would let a fresh branch seeded at main's head look
/// "merged".
fn collect_main_merge_tips(repo: &dyn Repository) -> Result<HashSet<Hash>> {
    const MAX_MAIN_WALK: usize = 100_000;

    let mut tips = HashSet::new();
    let Some(main_head) = repo.get_branch_head("main")? else {
        return Ok(tips);
    };

    let mut seen = HashSet::new();
    let mut current = Some(main_head);
    let mut walked = 0usize;
    while let Some(hash) = current {
        if walked >= MAX_MAIN_WALK || !seen.insert(hash.clone()) {
            break;
        }
        walked += 1;
        let Some(commit) = repo.get_commit(&hash)? else {
            break;
        };
        if let Some(tip) = commit.merge_parent_hash {
            tips.insert(tip);
        }
        current = commit.parent_hash;
    }
    Ok(tips)
}

/// Delete local rows for branches whose work already landed on `main` —
/// merged by this client in an earlier session, by another client, or in the
/// web UI. A branch is pruned when its head appears as a `merge_parent_hash`
/// in main's local history (the squash commit is the proof), or when it is
/// closed with no head at all. A branch with commits past its merged tip
/// keeps its row: the head no longer matches any squash commit, so the
/// unmerged work is never discarded. The current branch is always kept —
/// `apply_remote_merge_reconcile` owns that rotation.
///
/// Returns the pruned branch names. No-ops (returning what was pruned so
/// far) on backends without delete support.
pub(crate) fn prune_merged_branches(repo: &dyn Repository) -> Result<Vec<String>> {
    let merge_tips = collect_main_merge_tips(repo)?;
    let current = repo.get_current_branch_name()?;

    let mut pruned = Vec::new();
    for br in repo.list_branches()? {
        if br.name == oak_core::DEFAULT_BRANCH || current.as_deref() == Some(br.name.as_str()) {
            continue;
        }
        let merged = match repo.get_branch_head(&br.name)? {
            Some(head) => merge_tips.contains(&head),
            // A closed branch with no head has no commits to lose.
            None => br.status == BranchStatus::Closed,
        };
        if !merged {
            continue;
        }
        match repo.delete_branch(&br.name) {
            Ok(()) => pruned.push(br.name),
            Err(OakError::Unsupported { .. }) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(pruned)
}

pub(crate) fn print_pruned_branches(pruned: &[String]) {
    for name in pruned {
        output::item(&format!("removed merged branch '{name}'"));
    }
}

/// Reconstruct `main`'s new HEAD locally from a merge response, without
/// any network round-trips, when we already have everything we need.
///
/// The merge response carries the squash commit's hashes. When its
/// manifest is already in local storage — true whenever the merged tree
/// equals a manifest we've stored, i.e. the usual clean merge of an
/// up-to-date branch — we can synthesize the `main` commit row, ensure the
/// `main` branch row, and point it at the new head. By our storage
/// invariant a manifest is only ever stored after the blobs it references,
/// so holding the manifest means we hold the content too: no blob fetch,
/// no tree walk.
///
/// Returns `Some(head)` when the fast path applied, or `None` when the
/// caller should fall back to fetching main's tree from the server — the
/// merged manifest is new to us (main moved under the branch), or the
/// response came from an older server that didn't send the manifest hash.
///
/// Mirrors the commit-row synthesis in
/// [`super::sync::fetch_parent_from_server`]: the row's hash is the
/// server's authoritative commit hash, and `parent_hash` /
/// `merge_parent_hash` are carried through so the LCA finder can walk the
/// chain on later merges. Author / timestamp / message are placeholders —
/// they don't affect the content hash and aren't used locally.
fn try_ingest_merged_main(repo: &dyn Repository, merged: &MergeResponse) -> Result<Option<Hash>> {
    let Some(manifest_hash) = merged.manifest_hash.as_ref() else {
        return Ok(None);
    };
    let manifest_hash = Hash(manifest_hash.clone());
    if repo.get_manifest(&manifest_hash)?.is_none() {
        return Ok(None);
    }

    let head_hash = Hash(merged.commit_hash.clone());
    // Idempotent: a retry that already stored this commit just re-points
    // the branch row below.
    if repo.get_commit(&head_hash)?.is_none() {
        let commit = Commit {
            hash: head_hash.clone(),
            branch_name: "main".to_string(),
            parent_hash: merged.parent_hash.clone().map(Hash),
            merge_parent_hash: merged.merge_parent_hash.clone().map(Hash),
            manifest_hash,
            author: "<remote>".to_string(),
            message: None,
            timestamp: chrono::Utc::now(),
            files: Vec::new(),
        };
        repo.store_commit(&commit)?;
    }

    if repo.get_branch("main")?.is_none() {
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))?;
    }
    repo.set_branch_head("main", &head_hash)?;
    Ok(Some(head_hash))
}

/// Pick a name for a fresh personal branch that doesn't collide with any
/// existing branch row. Tries the author-derived default first, then
/// appends `-2`, `-3`, ... If everything up to `-99` is taken (vanishingly
/// unlikely), falls back to a timestamp suffix.
pub(crate) fn next_open_personal_branch_name(repo: &dyn Repository) -> Result<String> {
    let base = super::init::default_local_branch_name();
    if repo.get_branch(&base)?.is_none() {
        return Ok(base);
    }
    for n in 2..100 {
        let candidate = format!("{base}-{n}");
        if repo.get_branch(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", chrono::Utc::now().timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_conflict_paths_parsed_from_message() {
        let body = r#"{"error":"Merge conflict: 2 file(s) have overlapping edits on both branches: src/a.rs, src/b.rs"}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::MergeConflict(n) => assert_eq!(n, 2),
            other => panic!("expected MergeConflict, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_structured_paths_win_over_message() {
        let body =
            r#"{"error":"Merge conflict: stale message","conflict_paths":["x.rs","y.rs","z.rs"]}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::MergeConflict(n) => assert_eq!(n, 3),
            other => panic!("expected MergeConflict, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_without_paths_keeps_server_message() {
        let body = r#"{"error":"Merge conflict: branch is read-only"}"#;
        match server_merge_conflict_error("feature", body) {
            OakError::Server(msg) => {
                assert_eq!(msg, "Merge failed: Merge conflict: branch is read-only")
            }
            other => panic!("expected Server, got: {other}"),
        }
    }

    #[test]
    fn server_conflict_non_json_body_falls_back_to_generic() {
        match server_merge_conflict_error("feature", "<html>oops</html>") {
            OakError::Server(msg) => {
                assert_eq!(msg, "Merge failed: HTTP 409 Conflict: <html>oops</html>")
            }
            other => panic!("expected Server, got: {other}"),
        }
        match server_merge_conflict_error("feature", "  ") {
            OakError::Server(msg) => assert_eq!(msg, "Merge failed: HTTP 409 Conflict"),
            other => panic!("expected Server, got: {other}"),
        }
    }

    fn manifest_entry(repo: &dyn Repository, path: &str, content: &str) -> (ManifestEntry, Hash) {
        let blob_hash = repo.put_blob(content.as_bytes().to_vec()).unwrap();
        (
            ManifestEntry {
                path: path.to_string(),
                blob_hash: blob_hash.clone(),
                mode: oak_core::FileMode::Regular,
            },
            blob_hash,
        )
    }

    #[test]
    fn reconciles_current_branch_when_main_contains_remote_squash_merge() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        crate::commands::reset::reset_to_manifest(&lock, &root, &repo, &feature_manifest).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        assert_eq!(
            crate::commands::commit::unmerged_commit_count(&repo, "feature").unwrap(),
            1,
            "the old status path reports stale unmerged work"
        );

        let plan = plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
            .unwrap()
            .expect("current branch should reconcile after remote merge");
        let reconciled = apply_remote_merge_reconcile(&lock, &repo, &root, true, plan)
            .unwrap()
            .expect("planned reconciliation should rotate branches");

        assert_eq!(reconciled.closed_branch, "feature");
        assert!(reconciled.reset_worktree);
        assert!(
            repo.get_branch("feature").unwrap().is_none(),
            "remotely merged branch should be removed from local history"
        );

        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_eq!(current, reconciled.new_branch);
        assert_ne!(current, "feature");
        assert_eq!(
            repo.get_branch(&current)
                .unwrap()
                .unwrap()
                .parent_branch
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(main_after_web_merge.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(main_after_web_merge));

        let (changes, _, branch_name) = crate::commands::commit::compute_changes(&repo, &root)
            .expect("status computation should still work");
        assert!(changes.is_empty());
        assert_eq!(branch_name.as_deref(), Some(current.as_str()));
    }

    #[test]
    fn closed_current_branch_without_main_merge_proof_does_not_reconcile() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let main_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &main_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest_hash = repo.put_manifest(vec![base_entry, feature_entry]).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(main_head.clone()),
                None,
                feature_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        assert!(
            plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
                .unwrap()
                .is_none(),
            "closed branch alone is not enough proof of a web merge"
        );
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("feature")
        );
        assert_eq!(repo.get_head().unwrap(), Some(feature_head));
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Closed
        );
    }

    #[test]
    fn closed_current_branch_with_main_merge_proof_does_not_reconcile_again() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head.clone()),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        assert!(
            plan_remote_merge_reconcile(&repo, RemoteMergeReconcileScope::CurrentBranch)
                .unwrap()
                .is_none(),
            "switching back to a closed merged branch should not mint another branch"
        );
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("feature")
        );
        assert_eq!(repo.get_head().unwrap(), Some(feature_head));
        assert_eq!(
            repo.get_branch("feature").unwrap().unwrap().status,
            BranchStatus::Closed
        );
    }

    #[test]
    fn closed_current_branch_with_main_merge_proof_reconciles_when_open_at_pull_start() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("work");
        fs::create_dir_all(&root).unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, base_blob) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "README.md".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(base_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let mut feature =
            oak_core::Branch::new("feature".to_string(), None, Some("main".to_string()));
        feature.status = BranchStatus::Closed;
        repo.store_branch(&feature).unwrap();
        repo.set_current_branch("feature").unwrap();

        let (feature_entry, feature_blob) = manifest_entry(&repo, "feature.txt", "feature\n");
        let feature_manifest = Manifest::new(vec![base_entry, feature_entry]);
        let feature_manifest_hash = repo.put_manifest(feature_manifest.entries.clone()).unwrap();
        let feature_head = repo
            .put_commit(
                "feature".to_string(),
                Some(base_head.clone()),
                None,
                feature_manifest_hash.clone(),
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                vec![FileChange {
                    path: "feature.txt".to_string(),
                    change_type: oak_core::ChangeType::Added,
                    old_blob_hash: None,
                    new_blob_hash: Some(feature_blob),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(oak_core::FileMode::Regular),
                }],
            )
            .unwrap();
        repo.set_branch_head("feature", &feature_head).unwrap();
        repo.set_head(&feature_head).unwrap();

        let main_after_web_merge = repo
            .put_commit(
                "main".to_string(),
                Some(base_head),
                Some(feature_head.clone()),
                feature_manifest_hash,
                "<remote>".to_string(),
                Some("merged feature".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &main_after_web_merge).unwrap();

        let plan = plan_remote_merge_reconcile(
            &repo,
            RemoteMergeReconcileScope::Pull {
                branch_open_at_start: Some("feature"),
            },
        )
        .unwrap()
        .expect("pull should reconcile a branch that was open before server marked it closed");
        let lock = crate::workdir_lock::WorkdirLock::acquire(temp.path()).unwrap();
        let reconciled = apply_remote_merge_reconcile(&lock, &repo, &root, true, plan)
            .unwrap()
            .expect("planned pull reconciliation should rotate branches");

        assert_eq!(reconciled.closed_branch, "feature");
        assert!(reconciled.reset_worktree);
        assert!(
            repo.get_branch("feature").unwrap().is_none(),
            "remotely merged branch should be removed from local history"
        );
        let current = repo.get_current_branch_name().unwrap().unwrap();
        assert_eq!(current, reconciled.new_branch);
        assert_eq!(
            repo.get_branch_head(&current).unwrap(),
            Some(main_after_web_merge.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(main_after_web_merge));
    }

    /// The old `create_conflict_content` wrapped the entire file in one set
    /// of markers, so even a one-line divergence produced a ~50-line
    /// conflict. With diffy's 3-way merge, identical surrounding lines come
    /// through clean and only the diverging hunk is marked.
    #[test]
    fn conflict_content_marks_only_diverging_hunks() {
        let base = "alpha\nshared line\nomega\n";
        let ours = "alpha\nour change\nomega\n";
        let theirs = "alpha\ntheir change\nomega\n";

        let out = create_conflict_content(base, ours, theirs, "feature", "main");

        // Surrounding unchanged lines stay outside the markers.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.first().copied(), Some("alpha"));
        assert_eq!(lines.last().copied(), Some("omega"));

        // The marker labels are rewritten to the branch names.
        let feature_marker = format!("{OURS_MARKER} feature");
        let main_marker = format!("{THEIRS_MARKER} main");
        assert!(out.contains(&feature_marker), "actual: {out}");
        assert!(out.contains(&main_marker), "actual: {out}");

        // The unchanged lines are NOT inside the conflict block.
        let inside_conflict = out
            .split(&feature_marker)
            .nth(1)
            .and_then(|s| s.split(&main_marker).next())
            .unwrap_or("");
        assert!(
            !inside_conflict.contains("alpha"),
            "shared `alpha` leaked into conflict block: {inside_conflict}"
        );
        assert!(
            !inside_conflict.contains("omega"),
            "shared `omega` leaked into conflict block: {inside_conflict}"
        );
        assert!(inside_conflict.contains("our change"));
        assert!(inside_conflict.contains("their change"));
    }

    /// When both sides made the exact same change relative to base, diffy
    /// returns a clean merge — no markers in the output.
    #[test]
    fn conflict_content_clean_when_both_sides_agree() {
        let base = "alpha\nbeta\ngamma\n";
        let same = "alpha\nbeta changed\ngamma\n";

        let out = create_conflict_content(base, same, same, "feature", "main");
        assert!(!out.contains(OURS_MARKER), "expected clean merge: {out}");
        assert!(!out.contains(THEIRS_MARKER), "expected clean merge: {out}");
        assert_eq!(out.trim_end(), "alpha\nbeta changed\ngamma");
    }

    /// The regression this guards: both sides edit the *same file* but at
    /// distant, non-overlapping lines. diffy reconciles them into one clean
    /// file with no markers, so `content_has_conflict_markers` is false and the
    /// merge/sync/mount-pull call sites must NOT report a conflict. Before the
    /// fix, those sites flagged any both-sides-edited file regardless of
    /// overlap.
    #[test]
    fn conflict_content_clean_when_edits_dont_overlap() {
        let base = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
        let ours = "ALPHA-CHANGED\nbeta\ngamma\ndelta\nepsilon\n";
        let theirs = "alpha\nbeta\ngamma\ndelta\nEPSILON-CHANGED\n";

        let out = create_conflict_content(base, ours, theirs, "feature", "main");

        assert!(
            !content_has_conflict_markers(&out),
            "non-overlapping edits must merge clean, got:\n{out}"
        );
        // Both sides' changes survive in the merged result.
        assert!(out.contains("ALPHA-CHANGED"));
        assert!(out.contains("EPSILON-CHANGED"));
    }

    /// A real unresolved conflict has both markers at column 0 — flag it.
    #[test]
    fn conflict_markers_detected_at_column_zero() {
        let content = format!(
            "alpha\n{OURS_MARKER} feature\nour change\n=======\ntheir change\n{THEIRS_MARKER} main\nomega\n"
        );
        assert!(content_has_conflict_markers(&content));
    }

    /// Markers that appear only inside string literals / mid-line — as they do
    /// in oak's own source (doc comments, test fixtures, error messages) — are
    /// NOT real conflicts and must not be flagged.
    #[test]
    fn conflict_markers_ignored_when_not_at_column_zero() {
        // Indented (mid-line) markers, e.g. inside a string literal in code.
        let indented = format!("    let s = \"{OURS_MARKER} x\";\n    // {THEIRS_MARKER} y\n");
        assert!(!content_has_conflict_markers(&indented));

        // Both markers present but embedded inside a single source line.
        let embedded =
            format!("remove every {OURS_MARKER} / ======= / {THEIRS_MARKER} line before saving\n");
        assert!(!content_has_conflict_markers(&embedded));
    }

    /// Only one marker at column 0 (without its pair) is not a conflict.
    #[test]
    fn conflict_markers_require_both_sides() {
        let only_ours = format!("{OURS_MARKER} feature\nour change\n");
        assert!(!content_has_conflict_markers(&only_ours));
    }

    /// `prune_merged_branches` removes exactly the branches whose tips main's
    /// history records as squash-merged (plus headless closed rows), and
    /// keeps everything else: the current branch, branches with unmerged
    /// work, and fresh branches seeded at main's head.
    #[test]
    fn prune_removes_merged_branches_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let (base_entry, _) = manifest_entry(&repo, "README.md", "base\n");
        let base_manifest_hash = repo.put_manifest(vec![base_entry.clone()]).unwrap();
        let base_head = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                base_manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &base_head).unwrap();

        let new_branch = |name: &str| {
            repo.store_branch(&oak_core::Branch::new(
                name.to_string(),
                None,
                Some("main".to_string()),
            ))
            .unwrap();
        };
        let commit_on = |name: &str, file: &str, parent: &Hash| {
            let (entry, _) = manifest_entry(&repo, file, file);
            let manifest_hash = repo.put_manifest(vec![base_entry.clone(), entry]).unwrap();
            repo.put_commit(
                name.to_string(),
                Some(parent.clone()),
                None,
                manifest_hash,
                "tester".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap()
        };

        // Merged on main (web UI / another client): tip recorded as the
        // squash commit's merge_parent_hash.
        new_branch("merged");
        let merged_tip = commit_on("merged", "merged.txt", &base_head);
        repo.set_branch_head("merged", &merged_tip).unwrap();
        let squash = repo
            .put_commit(
                "main".to_string(),
                Some(base_head.clone()),
                Some(merged_tip.clone()),
                repo.get_commit(&merged_tip).unwrap().unwrap().manifest_hash,
                "<remote>".to_string(),
                Some("merged".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.set_branch_head("main", &squash).unwrap();

        // Merged, but with a local commit past the merged tip — must be kept.
        new_branch("merged-plus-work");
        repo.set_branch_head("merged-plus-work", &merged_tip)
            .unwrap();
        let extra = commit_on("merged-plus-work", "extra.txt", &merged_tip);
        repo.set_branch_head("merged-plus-work", &extra).unwrap();

        // Fresh branch seeded at main's new head: head is a main commit, not
        // a merged tip — must be kept.
        new_branch("fresh");
        repo.set_branch_head("fresh", &squash).unwrap();

        // Closed tombstone without a head: nothing to lose — pruned.
        let mut headless =
            oak_core::Branch::new("headless".to_string(), None, Some("main".to_string()));
        headless.status = BranchStatus::Closed;
        repo.store_branch(&headless).unwrap();

        // Current branch is never pruned, even at the merged tip.
        new_branch("current");
        repo.set_branch_head("current", &merged_tip).unwrap();
        repo.set_current_branch("current").unwrap();

        let mut pruned = prune_merged_branches(&repo).unwrap();
        pruned.sort();
        assert_eq!(pruned, vec!["headless".to_string(), "merged".to_string()]);

        assert!(repo.get_branch("merged").unwrap().is_none());
        assert!(repo.get_branch("headless").unwrap().is_none());
        assert!(repo.get_branch("merged-plus-work").unwrap().is_some());
        assert!(repo.get_branch("fresh").unwrap().is_some());
        assert!(repo.get_branch("current").unwrap().is_some());
        assert!(repo.get_branch("main").unwrap().is_some());
        assert!(
            repo.get_branch_head("merged").unwrap().is_none(),
            "pruned branch's head pointer should be deleted too"
        );
    }
}
