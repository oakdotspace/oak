use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{
    three_way_merge_manifests, Blob, Commit, FileChange, FileMode, Hash, IgnorePatterns, Manifest,
    ManifestEntry, MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};

use crate::atomic_file;
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// On-disk record (`.oak/SYNC_STATE`, JSON) of a conflicted parent-sync,
/// written next to SYNC_HEAD when the 3-way merge pauses for manual
/// resolution. It pins the merge result (`merged_manifest_hash`, stored in
/// the DB) and which paths actually conflicted, so `oak pull --continue`
/// can commit exactly those — instead of snapshotting the whole working
/// tree and sweeping unrelated dirty files into the sync commit. A sync
/// paused by an older binary has no SYNC_STATE; `sync_continue` falls back
/// to the legacy whole-tree scan for it.
#[derive(Serialize, Deserialize)]
struct SyncState {
    merged_manifest_hash: String,
    conflict_paths: Vec<String>,
}

/// Sync the current branch with changes from its parent branch.
///
/// Async because parents that live only on the server (notably `main`) are
/// fetched on demand: the local DB doesn't carry a `main` branch row, so a
/// sync from `main` first downloads its head commit + manifest + any
/// missing blobs from the server, then runs the regular local 3-way merge.
///
/// This is the second half of `oak pull` (the first half fetches the
/// current branch from the server). Standalone `oak sync` was removed —
/// the orchestrator in `pull::run` is the only caller.
///
/// Returns `Some` when refreshing `main` proves the current branch was already
/// squash-merged remotely and local branch rotation completed instead of
/// creating a sync commit on the old branch.
pub async fn sync_from_parent(path: &Path) -> Result<()> {
    sync_from_parent_for_pull(path, None).await.map(|_| ())
}

pub(crate) async fn sync_from_parent_for_pull(
    path: &Path,
    branch_open_at_pull_start: Option<&str>,
) -> Result<Option<super::merge::RemoteMergeReconcile>> {
    let ctx = crate::resolve::resolve(path)?;
    let root = &ctx.work_tree;

    // Check if a sync or merge is already in progress
    if root.join(".oak/SYNC_HEAD").exists() {
        return Err(OakError::MergeFailed(
            "A sync is already in progress. Use 'oak pull --continue' or 'oak pull --abort'."
                .to_string(),
        ));
    }
    if root.join(".oak/MERGE_HEAD").exists() {
        return Err(OakError::MergeInProgress);
    }

    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    // Everything below — remote-merge reconciliation, the conflict
    // write-out, and `complete_sync`'s reset — may rewrite the working
    // tree. Hold the workdir lock for the whole phase; it was previously
    // only held for the reconcile path, leaving the other mutations racing
    // any concurrent `oak commit`.
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;

    // Get current branch name
    let branch_name = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;

    // Get the branch and verify it has a parent
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;

    let parent_name = branch.parent_branch.ok_or_else(|| {
        OakError::BranchNotFound(format!("branch '{branch_name}' has no parent to sync from"))
    })?;

    // Get the current branch's head commit
    let branch_head = repo
        .get_branch_head(&branch_name)?
        .ok_or(OakError::NoCommits)?;

    let branch_commit = repo.get_commit(&branch_head)?.ok_or_else(|| {
        OakError::Server(format!(
            "Branch head commit {} not in local storage. \
             Run 'oak pull --force' to recover.",
            &branch_head.0[..12.min(branch_head.0.len())]
        ))
    })?;

    // Parent's head. The local DB doesn't carry a `main` branch row by
    // design (schema comment in branches table), and earlier versions of
    // this code also left stale `branch_heads` rows behind when the
    // remote moved on. Always re-fetch the parent's HEAD from the
    // server when it lives there — local state for `main` is unreliable
    // and using a stale head silently produces a sync commit pointing at
    // the wrong LCA, which later merges then 409 on.
    let parent_head = if parent_name == "main" {
        fetch_parent_from_server(&repo, &parent_name).await?
    } else {
        repo.get_branch_head(&parent_name)?
    };

    // If the parent branch has no commits yet (e.g. `main` hasn't been
    // bootstrapped by any squash-merge), there's nothing to sync into this
    // branch. Leave it untouched — treating the parent as an empty manifest
    // would look like "the parent deleted every file" and spuriously rewrite
    // the working tree.
    if parent_head.is_none() {
        output::info("Parent branch has no commits yet — nothing to sync.");
        return Ok(None);
    }

    if parent_name == "main" {
        if let Some(plan) = super::merge::plan_remote_merge_reconcile(
            &repo,
            super::merge::RemoteMergeReconcileScope::Pull {
                branch_open_at_start: branch_open_at_pull_start,
            },
        )? {
            let worktree_clean =
                super::commit::worktree_is_clean_without_storing_blobs(&repo, root)?;
            if let Some(reconciled) = super::merge::apply_remote_merge_reconcile(
                &lock,
                &repo,
                root,
                worktree_clean,
                plan,
            )? {
                return Ok(Some(reconciled));
            }
        }
    }

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

    // Find the LCA manifest, passing the parent head that was just fetched from server.
    // This ensures that even when the parent is "main" (which has no local branch row),
    // the LCA finder can walk the parent's history correctly.
    let base_manifest = super::merge::find_lca_manifest_with_parent(
        &repo,
        &branch_name,
        &parent_name,
        parent_head.as_ref(),
    )?;

    // Path-by-path 3-way merge — the same `three_way_merge_manifests` that
    // `oak merge` and the server use, so a file's merge identity is
    // `(content, mode)` everywhere. The previous hand-rolled copy here
    // compared blob hashes only, which silently reset executable bits on
    // every pull that followed a squash to main. Conflict policy (markers,
    // binary keeps branch, modify/delete keeps the survivor) is the shared
    // CLI resolver from merge.rs.
    let outcome = three_way_merge_manifests(&base_manifest, &branch_manifest, &parent_manifest);
    let mut merged_entries = outcome.clean_entries;
    let resolution = super::merge::resolve_conflicts_with_markers(
        &repo,
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
        let merged_manifest = Manifest::new(merged_entries);
        write_sync_conflict_state(
            &lock,
            &repo,
            root,
            &branch_name,
            &parent_name,
            parent_head.as_ref(),
            None,
            &merged_manifest,
            &conflict_paths,
            &binary_conflict_paths,
        )?;
        return Err(OakError::MergeConflict(total_conflicts));
    }

    // No conflicts - complete the sync
    let merged_manifest = Manifest::new(merged_entries);

    // If the merge result is byte-for-byte the current branch tip, the parent
    // had nothing to contribute — don't manufacture an empty sync commit.
    // Doing so would advance this branch's head past a peer's identical head
    // and break idempotency (`oak pull` twice should be a no-op the second
    // time).
    if merged_manifest.hash == branch_manifest.hash {
        output::info("Already up to date with parent.");
        return Ok(None);
    }

    complete_sync(
        &lock,
        &repo,
        root,
        &branch_name,
        &parent_name,
        Some(branch_head),
        parent_head.clone(),
        &merged_manifest,
        true,
    )?;

    Ok(None)
}

/// Complete the sync: create commit on the current branch, update head.
///
/// `merge_parent` is the parent branch's HEAD at sync time. Stamping it on
/// the sync commit's `merge_parent_hash` lets the LCA finder walk *through*
/// the sync commit into the parent's history when computing future merges,
/// turning a "both branches modified file X" conflict into a clean
/// fast-forward of the branch's resolution.
///
/// `update_workdir` selects whether the working tree is reset to the merged
/// manifest afterwards. The no-conflict path needs that (the tree still
/// shows pre-merge content); the conflict-resolution path must NOT — there
/// the tree already holds the merge write-out plus the user's resolutions,
/// and a full reset would clobber the unrelated dirty files the scoped
/// commit deliberately left uncommitted.
#[allow(clippy::too_many_arguments)]
fn complete_sync(
    lock: &WorkdirLock,
    repo: &SqliteRepository,
    root: &Path,
    branch_name: &str,
    parent_name: &str,
    branch_head: Option<Hash>,
    merge_parent: Option<Hash>,
    merged_manifest: &Manifest,
    update_workdir: bool,
) -> Result<()> {
    // Diff against the current branch's manifest to get file changes
    let current_manifest = if let Some(ref bh) = branch_head {
        let bc = repo
            .get_commit(bh)?
            .ok_or_else(|| OakError::CommitNotFound(bh.to_string()))?;
        repo.get_manifest(&bc.manifest_hash)?
            .unwrap_or_else(Manifest::empty)
    } else {
        Manifest::empty()
    };

    let changes = current_manifest.diff(merged_manifest);
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

    let author = super::commit::get_author();

    // Create sync commit on the CURRENT branch (not the parent). Sync
    // commits don't carry a message — feature-branch commits never do.
    // `merge_parent_hash = parent's HEAD at sync time` is what makes a
    // later merge of this branch into the parent find a recent LCA
    // (otherwise the LCA finder falls back to the original fork point
    // and any file modified on both sides since then becomes a conflict).
    let sync_commit_hash = repo.put_commit_and_advance_refs(
        branch_name.to_string(),
        branch_head,
        merge_parent,
        merged_manifest.entries.clone(),
        author,
        None,
        chrono::Utc::now(),
        file_changes,
    )?;

    // Update working directory to synced state
    if update_workdir {
        crate::commands::reset::reset_to_manifest(lock, root, repo, merged_manifest)?;
    }

    output::success(&format!(
        "Synced branch '{branch_name}' from '{parent_name}'"
    ));
    output::info(&format!("  commit {}", sync_commit_hash.short()));

    Ok(())
}

/// Persist a paused (conflicted) sync: write the conflicted tree to the
/// working directory, record the merge result (manifest + SYNC_STATE) so
/// `oak pull --continue` commits exactly "merge result + the user's
/// resolutions", and write SYNC_HEAD/SYNC_MSG. Shared by the parent-sync
/// phase of `oak pull` and the snapshot re-parent path.
///
/// SYNC_HEAD lines: parent branch name, branch name, the parent's HEAD at
/// sync time (stamped as `merge_parent_hash` on the resolved commit), and —
/// only for a re-parent — a fourth line holding the branch's old tip. The
/// fourth line tells `sync_continue` to parent the resolved commit on the
/// recorded parent head (re-seeding the branch there) and to stamp the old
/// tip as `merge_parent_hash` instead, so the pre-re-parent history stays
/// reachable. Older SYNC_HEAD files (two or three lines) keep their existing
/// behavior.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_sync_conflict_state(
    lock: &WorkdirLock,
    repo: &SqliteRepository,
    root: &Path,
    branch_name: &str,
    parent_name: &str,
    parent_head: Option<&Hash>,
    reseed_old_tip: Option<&Hash>,
    merged_manifest: &Manifest,
    conflict_paths: &[String],
    binary_conflict_paths: &[String],
) -> Result<()> {
    super::merge::write_manifest_to_workdir(lock, root, repo, merged_manifest)?;

    // Persist the merge result itself (the manifest object plus a
    // SYNC_STATE record of its hash and the conflicted paths). That's
    // what lets `sync_continue` commit exactly "3-way merge result +
    // the user's conflict resolutions" instead of snapshotting the
    // whole working tree — which used to silently sweep every
    // unrelated dirty file into the sync commit. Written before
    // SYNC_HEAD so a crash between the two leaves no SYNC_STATE-less
    // half-state behind (SYNC_HEAD is what `--continue` keys off).
    repo.store_manifest(merged_manifest)?;
    let state = SyncState {
        merged_manifest_hash: merged_manifest.hash.0.clone(),
        conflict_paths: conflict_paths
            .iter()
            .chain(binary_conflict_paths.iter())
            .cloned()
            .collect(),
    };
    let state_json =
        serde_json::to_string_pretty(&state).map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    atomic_file::write_atomic(&root.join(".oak/SYNC_STATE"), state_json)?;

    let sync_head_path = root.join(".oak/SYNC_HEAD");
    let sync_msg_path = root.join(".oak/SYNC_MSG");
    let parent_head_line = parent_head.map(|h| h.0.as_str()).unwrap_or_default();
    let mut sync_head = format!("{parent_name}\n{branch_name}\n{parent_head_line}");
    if let Some(old_tip) = reseed_old_tip {
        sync_head.push('\n');
        sync_head.push_str(old_tip.as_str());
    }
    atomic_file::write_atomic(&sync_head_path, sync_head)?;
    atomic_file::write_atomic(
        &sync_msg_path,
        format!("Sync branch '{branch_name}' from '{parent_name}'"),
    )?;

    let total_conflicts = conflict_paths.len() + binary_conflict_paths.len();
    output::warning(&format!(
        "Sync conflict: {total_conflicts} file(s) need manual resolution"
    ));
    for p in conflict_paths {
        output::info(&format!("  CONFLICT (content): {p}"));
    }
    for p in binary_conflict_paths {
        output::info(&format!("  CONFLICT (binary): {p} - kept branch version"));
    }
    output::info("");
    output::info("Fix the conflicts and then run 'oak pull --continue'");
    output::info("To abort the sync, run 'oak pull --abort'");

    Ok(())
}

/// Continue a sync after conflicts have been resolved.
/// Called by `oak pull --continue`.
pub fn sync_continue(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    let sync_head_path = root.join(".oak/SYNC_HEAD");
    let sync_msg_path = root.join(".oak/SYNC_MSG");

    if !sync_head_path.exists() {
        return Err(OakError::MergeFailed("No sync in progress.".to_string()));
    }

    // Read sync state. Line 3 (`parent_head`) is optional — older SYNC_HEAD
    // files written before this field existed only have two lines; we
    // tolerate that and fall back to `merge_parent_hash = None` for the
    // resolved commit (preserving prior behavior).
    let sync_head_content = fs::read_to_string(&sync_head_path)?;
    let mut lines = sync_head_content.lines();
    let parent_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt SYNC_HEAD".to_string()))?
        .to_string();
    let branch_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt SYNC_HEAD".to_string()))?
        .to_string();
    let merge_parent = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Hash(s.to_string()));
    // Line 4 (only written by the re-parent path): the branch's pre-re-parent
    // tip. Its presence means the resolved commit must be parented on the
    // recorded parent head (line 3) — re-seeding the branch there — with the
    // old tip stamped as merge_parent so prior history stays reachable.
    let reseed_old_tip = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Hash(s.to_string()));

    // Scan working directory for remaining conflict markers
    let ignore = IgnorePatterns::new(root)?;
    let conflicted = super::merge::find_conflict_markers(root, root, &ignore)?;

    if !conflicted.is_empty() {
        output::error(&format!(
            "{} file(s) still have conflict markers:",
            conflicted.len()
        ));
        for p in &conflicted {
            output::info(&format!("  {p}"));
        }
        output::info("Edit these files to resolve conflicts, then run 'oak pull --continue'");
        return Err(OakError::MergeConflict(conflicted.len()));
    }

    // Re-hash the working tree from disk (no-cache scan): the recorded sync
    // commit must reflect on-disk content and can never echo a stale
    // path-keyed stat-cache row from another branch's version of a file
    // (the foreign-blob bug). This path runs once per conflicted pull, so
    // the extra hashing is cheap.
    let entries = crate::commands::commit::scan_working_dir_no_cache(root, root, &repo, &ignore)?;
    let scanned = Manifest::new(entries);

    let branch_head = repo.get_branch_head(&branch_name)?;

    // Scope the sync commit to the recorded merge result plus the user's
    // conflict resolutions. Without SYNC_STATE (sync paused by an older
    // binary) fall back to the legacy whole-tree snapshot — which sweeps
    // unrelated dirty files into the sync commit, the behavior SYNC_STATE
    // exists to prevent.
    let sync_state_path = root.join(".oak/SYNC_STATE");
    let state: Option<SyncState> = fs::read_to_string(&sync_state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let recorded_merge: Option<Manifest> = state
        .as_ref()
        .and_then(|s| {
            repo.get_manifest(&Hash(s.merged_manifest_hash.clone()))
                .ok()
        })
        .flatten();

    let (final_manifest, update_workdir) = match (&state, recorded_merge) {
        (Some(state), Some(merged)) => {
            let scoped = scope_sync_commit_to_merge(
                &lock,
                &repo,
                root,
                &ignore,
                &merged,
                &scanned,
                &state.conflict_paths,
                branch_head.as_ref(),
            )?;
            // The tree already holds the merge write-out, the user's
            // resolutions, and their unrelated dirty files — don't reset it.
            (scoped, false)
        }
        _ => (scanned, true),
    };

    // A re-parent sync parents the resolved commit on the recorded parent
    // head (line 3) and keeps the old tip reachable via merge_parent_hash;
    // a regular sync parents on the branch's tip as before.
    let (commit_parent, commit_merge_parent) = match reseed_old_tip {
        Some(old_tip) => (merge_parent, Some(old_tip)),
        None => (branch_head, merge_parent),
    };

    complete_sync(
        &lock,
        &repo,
        root,
        &branch_name,
        &parent_name,
        commit_parent,
        commit_merge_parent,
        &final_manifest,
        update_workdir,
    )?;

    // Clean up sync state files
    fs::remove_file(&sync_head_path).ok();
    fs::remove_file(&sync_msg_path).ok();
    fs::remove_file(&sync_state_path).ok();

    Ok(())
}

/// Build the manifest a conflicted sync should actually commit: the recorded
/// 3-way merge result with each conflicted path replaced by the user's
/// on-disk resolution (a conflict path missing from disk was resolved by
/// deleting the file).
///
/// Everything else the whole-tree scan found is deliberately NOT committed:
/// a parent-sync is a merge, not a snapshot, and unrelated dirty files —
/// edits in flight, untracked scratch files, or working-tree corruption left
/// behind by a buggy clone — must stay uncommitted local changes. They're
/// reported loudly so a later plain `oak commit` (which IS a whole-tree
/// snapshot) doesn't pick them up unnoticed.
///
/// Files the merge dropped (parent deleted them) that sit on disk untouched
/// are removed here, the same way a fast-forward pull applies upstream
/// deletions — otherwise they'd linger as untracked files and resurrect on
/// the next commit. A dropped path with local modifications is kept on disk
/// and reported instead.
#[allow(clippy::too_many_arguments)]
fn scope_sync_commit_to_merge(
    _lock: &WorkdirLock,
    repo: &SqliteRepository,
    root: &Path,
    ignore: &IgnorePatterns,
    merged: &Manifest,
    scanned: &Manifest,
    conflict_paths: &[String],
    branch_head: Option<&Hash>,
) -> Result<Manifest> {
    use std::collections::{HashMap, HashSet};

    let conflicts: HashSet<&str> = conflict_paths.iter().map(|s| s.as_str()).collect();
    let disk: HashMap<&str, &ManifestEntry> = scanned
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let merged_by_path: HashMap<&str, &ManifestEntry> = merged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut entries: Vec<ManifestEntry> = Vec::with_capacity(merged.entries.len());
    for entry in &merged.entries {
        if conflicts.contains(entry.path.as_str()) {
            if let Some(resolved) = disk.get(entry.path.as_str()) {
                entries.push((*resolved).clone());
            }
            // else: resolved by deletion — drop the entry.
        } else {
            entries.push(entry.clone());
        }
    }
    let final_manifest = Manifest::new(entries);
    let final_paths: HashSet<&str> = final_manifest
        .entries
        .iter()
        .map(|e| e.path.as_str())
        .collect();

    // Apply the merge's deletions to disk first: tracked at the branch tip,
    // dropped from the merge result, and on disk unmodified. These are the
    // parent's deletions landing, not local changes — they must not show up
    // in the dirty-files report below. A dropped path with local
    // modifications is left on disk and reported instead.
    let mut deleted: HashSet<String> = HashSet::new();
    if let Some(bh) = branch_head {
        let branch_manifest = repo
            .get_commit(bh)?
            .and_then(|c| repo.get_manifest(&c.manifest_hash).ok().flatten())
            .unwrap_or_else(Manifest::empty);
        for entry in &branch_manifest.entries {
            if final_paths.contains(entry.path.as_str()) {
                continue;
            }
            let Some(on_disk) = disk.get(entry.path.as_str()) else {
                continue; // already gone from disk
            };
            if on_disk.blob_hash != entry.blob_hash || on_disk.mode != entry.mode {
                continue; // locally modified — reported as dirty, kept
            }
            let file_path = root.join(&entry.path);
            match fs::symlink_metadata(&file_path) {
                // lstat: delete the file or symlink itself, never what a
                // symlink points at; a directory here means the path was
                // replaced — leave it for the next scan to report.
                Ok(md) if !md.is_dir() => fs::remove_file(&file_path)?,
                _ => continue,
            }
            deleted.insert(entry.path.clone());
        }
    }
    if !deleted.is_empty() {
        // The no-cache scan above seeded rows for these paths; drop them
        // along with the files.
        let deleted_list: Vec<String> = deleted.iter().cloned().collect();
        repo.update_stat_cache(&[], &deleted_list)?;
        crate::materialize::prune_emptied_dirs(root, ignore, &deleted_list)?;
    }

    // Dirty files outside the conflict set: on disk but diverging from the
    // merge result (modified or brand new), or in the merge result but gone
    // from disk. All stay uncommitted.
    let mut dirty: Vec<String> = Vec::new();
    for entry in &scanned.entries {
        if conflicts.contains(entry.path.as_str()) || deleted.contains(&entry.path) {
            continue;
        }
        match merged_by_path.get(entry.path.as_str()) {
            Some(m) if m.blob_hash == entry.blob_hash && m.mode == entry.mode => {}
            _ => dirty.push(entry.path.clone()),
        }
    }
    for entry in &merged.entries {
        if !conflicts.contains(entry.path.as_str()) && !disk.contains_key(entry.path.as_str()) {
            dirty.push(format!("{} (locally deleted)", entry.path));
        }
    }

    if !dirty.is_empty() {
        dirty.sort();
        output::warning(&format!(
            "{} file(s) with local changes unrelated to this sync were NOT \
             included in the sync commit and remain uncommitted:",
            dirty.len()
        ));
        const LIST_LIMIT: usize = 10;
        for path in dirty.iter().take(LIST_LIMIT) {
            output::info(&format!("  {path}"));
        }
        if dirty.len() > LIST_LIMIT {
            output::info(&format!("  … and {} more", dirty.len() - LIST_LIMIT));
        }
        output::info("Review them with 'oak status'; a plain 'oak commit' will include them.");
    }

    Ok(final_manifest)
}

/// Abort a sync in progress.
/// Called by `oak pull --abort`.
pub fn sync_abort(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = WorkdirLock::acquire(&ctx.oak_dir)?;
    let root = &ctx.work_tree;
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    let sync_head_path = root.join(".oak/SYNC_HEAD");
    let sync_msg_path = root.join(".oak/SYNC_MSG");

    if !sync_head_path.exists() {
        return Err(OakError::MergeFailed("No sync in progress.".to_string()));
    }

    // Read sync state to get the current branch name
    let sync_head_content = fs::read_to_string(&sync_head_path)?;
    let mut lines = sync_head_content.lines();
    let _parent_name = lines.next();
    let branch_name = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt SYNC_HEAD".to_string()))?
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
        crate::commands::reset::reset_to_manifest(&lock, root, &repo, &manifest)?;
    }

    // Clean up sync state files
    fs::remove_file(&sync_head_path).ok();
    fs::remove_file(&sync_msg_path).ok();
    fs::remove_file(root.join(".oak/SYNC_STATE")).ok();

    output::success("Sync aborted, working directory restored");

    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot re-parenting (Invariants 2 + 3).
//
// A branch becomes "diverged" when its locally recorded history isn't
// anchored on the server's branch head — most commonly because the branch's
// server-side seed moved when `main` advanced under it (no real remote
// work), or because local `main` once held a synthetic (non-canonical)
// commit identity. Both `oak pull` (on the server's 409) and `oak push` (on
// a remote head missing locally) converge through the same move: a per-path
// 3-way overlay of the branch's changes onto the server's snapshot, recorded
// as ONE new commit parented on the server's head. Cost is O(changed paths);
// no commit-graph surgery, and no commit is ever discarded — the old tip
// stays reachable via the new commit's merge_parent_hash.
// ---------------------------------------------------------------------------

/// What `prepare_reparent` found out about the diverged branch.
pub(crate) enum ReparentCheck {
    /// The server's branch head is already in local history — local is
    /// simply ahead (unpushed commits). Nothing to re-parent.
    AlreadyAnchored,
    /// Re-parenting is possible; apply with [`complete_reparent`] (or route
    /// the conflicts into the existing `--continue` flow).
    Ready(Box<ReparentPlan>),
}

pub(crate) struct ReparentPlan {
    /// The server's branch head — the commit the re-seeded branch extends.
    pub seed: Hash,
    /// `seed`'s snapshot ("theirs" in the 3-way).
    pub seed_manifest: Manifest,
    /// Branch name of the `seed` commit as recorded on the server. `"main"`
    /// for a moved seed (the dominant case); the pushed branch's own name
    /// when the remote branch holds real foreign commits.
    pub seed_branch_name: String,
    /// The branch's local tip before re-parenting (kept reachable).
    pub old_tip: Hash,
    /// Overlay result: ours applied onto the seed snapshot.
    pub merged_entries: Vec<ManifestEntry>,
    pub conflict_paths: Vec<String>,
    pub binary_conflict_paths: Vec<String>,
}

impl ReparentPlan {
    pub(crate) fn conflict_count(&self) -> usize {
        self.conflict_paths.len() + self.binary_conflict_paths.len()
    }
}

/// Walk `from`'s local ancestry (parent + merge-parent edges) looking for
/// `target`. Bounded; missing rows just end their chain.
pub(crate) fn local_history_contains(
    repo: &dyn Repository,
    from: &Hash,
    target: &Hash,
) -> Result<bool> {
    const MAX_WALK: usize = 100_000;
    let mut queue = std::collections::VecDeque::from([from.clone()]);
    let mut seen = std::collections::HashSet::new();
    let mut walked = 0usize;
    while let Some(hash) = queue.pop_front() {
        if walked >= MAX_WALK || !seen.insert(hash.0.clone()) {
            continue;
        }
        walked += 1;
        if &hash == target {
            return Ok(true);
        }
        if let Some(commit) = repo.get_commit(&hash)? {
            if let Some(p) = commit.parent_hash {
                queue.push_back(p);
            }
            if let Some(mp) = commit.merge_parent_hash {
                queue.push_back(mp);
            }
        }
    }
    Ok(false)
}

/// Fetch the server's branch head, materialize its snapshot locally
/// (commit row verbatim, tree objects, missing blobs), and compute the
/// 3-way overlay of the branch's changes onto it.
///
/// base = the branch's fork-point snapshot (via the LCA finder, falling back
/// to the parent of the branch's first own commit), ours = the branch tip's
/// manifest, theirs = the server head's manifest.
///
/// Read-mostly: the only local mutations are content-addressed object
/// ingestion (commit/trees/blobs of the server head) and — when the head is
/// a `main` commit — the Invariant-1 reconcile swap. Branch pointers move
/// only in [`complete_reparent`].
pub(crate) async fn prepare_reparent(
    repo: &SqliteRepository,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
    api_key: Option<&str>,
) -> Result<ReparentCheck> {
    let old_tip = repo
        .get_branch_head(branch_name)?
        .ok_or(OakError::NoCommits)?;

    // The commit the next push must extend: the server's branch head. A
    // branch the server doesn't know yet anchors on main's head instead.
    let client = crate::http::api_client();
    let endpoint = format!("{owner}/{repo_name}");
    let seed = match super::push::fetch_remote_branch_head(
        &client,
        remote,
        &endpoint,
        branch_name,
        api_key,
    )
    .await?
    {
        Some(seed) => seed,
        None => match fetch_parent_from_server(repo, oak_core::DEFAULT_BRANCH).await? {
            Some(head) => head,
            // Neither the branch nor main exists remotely — there is no
            // remote history to diverge from.
            None => return Ok(ReparentCheck::AlreadyAnchored),
        },
    };

    if seed == old_tip || local_history_contains(repo, &old_tip, &seed)? {
        return Ok(ReparentCheck::AlreadyAnchored);
    }

    // Materialize the seed commit: server-verbatim row + tree objects via
    // /commits/info, then any blobs its manifest references.
    if repo.get_commit(&seed)?.is_none() {
        super::blob_fetch::ensure_commits_local(
            repo,
            remote,
            owner,
            repo_name,
            api_key,
            std::slice::from_ref(&seed),
        )
        .await?;
    }
    let seed_commit = repo.get_commit(&seed)?.ok_or_else(|| {
        OakError::Server(format!(
            "remote branch head {} could not be fetched from the server",
            seed.short()
        ))
    })?;
    let seed_manifest = repo
        .get_manifest(&seed_commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(seed_commit.manifest_hash.to_string()))?;
    let blob_hashes: Vec<Hash> = seed_manifest
        .entries
        .iter()
        .map(|e| e.blob_hash.clone())
        .collect();
    super::blob_fetch::ensure_blobs_local(repo, remote, owner, repo_name, api_key, &blob_hashes)
        .await?;

    // Canonical-main bookkeeping: a moved seed IS a main commit. Record it
    // as local main's head (when main has none yet) and repoint anything
    // still anchored on a synthetic duplicate of it.
    if seed_commit.branch_name == oak_core::DEFAULT_BRANCH {
        ensure_branch_row(repo, oak_core::DEFAULT_BRANCH)?;
        if repo.get_branch_head(oak_core::DEFAULT_BRANCH)?.is_none() {
            repo.set_branch_head(oak_core::DEFAULT_BRANCH, &seed)?;
        }
        reconcile_synthetic_main(repo, &seed)?;
    }

    // 3-way overlay: base = fork-point snapshot, ours = branch tip, theirs
    // = the seed snapshot. Same merge + conflict policy as `oak merge` and
    // the parent-sync phase, so a file's merge identity can't drift.
    let ours_commit = repo
        .get_commit(&old_tip)?
        .ok_or_else(|| OakError::CommitNotFound(old_tip.to_string()))?;
    let ours = repo
        .get_manifest(&ours_commit.manifest_hash)?
        .unwrap_or_else(Manifest::empty);
    let base = super::merge::find_lca_manifest_with_parent(
        repo,
        branch_name,
        oak_core::DEFAULT_BRANCH,
        Some(&seed),
    )?;
    let outcome = three_way_merge_manifests(&base, &ours, &seed_manifest);
    let mut merged_entries = outcome.clean_entries;
    let resolution = super::merge::resolve_conflicts_with_markers(
        repo,
        &base,
        outcome.conflicts,
        branch_name,
        &seed_commit.branch_name,
    )?;
    merged_entries.extend(resolution.entries);

    Ok(ReparentCheck::Ready(Box::new(ReparentPlan {
        seed,
        seed_manifest,
        seed_branch_name: seed_commit.branch_name,
        old_tip,
        merged_entries,
        conflict_paths: resolution.conflict_paths,
        binary_conflict_paths: resolution.binary_conflict_paths,
    })))
}

pub(crate) struct ReparentResult {
    pub commit: Hash,
    pub reset_worktree: bool,
}

/// Apply a conflict-free [`ReparentPlan`]: record the overlay as one new
/// commit parented on the seed (old tip kept reachable via
/// `merge_parent_hash`), move the branch there, and — when the working tree
/// was clean and a workdir lock is held — reset the tree to the overlay.
/// A dirty tree is left untouched, exactly like `oak switch -c` carrying
/// dirty files onto a new branch.
pub(crate) fn complete_reparent(
    lock: Option<&WorkdirLock>,
    repo: &SqliteRepository,
    root: &Path,
    branch_name: &str,
    plan: &ReparentPlan,
    worktree_clean: bool,
) -> Result<ReparentResult> {
    let merged = Manifest::new(plan.merged_entries.clone());
    let changes = plan.seed_manifest.diff(&merged);
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

    let commit_hash = repo.put_commit_and_advance_refs(
        branch_name.to_string(),
        Some(plan.seed.clone()),
        Some(plan.old_tip.clone()),
        merged.entries.clone(),
        super::commit::get_author(),
        None,
        chrono::Utc::now(),
        file_changes,
    )?;

    let reset_worktree = worktree_clean && lock.is_some();
    if reset_worktree {
        if let Some(lock) = lock {
            crate::commands::reset::reset_to_manifest(lock, root, repo, &merged)?;
        }
    }

    Ok(ReparentResult {
        commit: commit_hash,
        reset_worktree,
    })
}

// ---------------------------------------------------------------------------
// Server fetch path (used when the parent branch isn't materialized locally,
// or its local head is stale — both are true for `main` in practice).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RepoInfoResponse {
    #[serde(default)]
    head: Option<String>,
}

#[derive(Deserialize)]
struct TreeEntryResponse {
    name: String,
    path: String,
    kind: String, // "blob" | "tree"
    blob_hash: String,
}

/// Fetch the parent branch's HEAD commit + manifest + missing blobs from
/// the remote, materialize them in local storage, and return the head
/// hash. Always re-reads from the server — local state for `main` is
/// unreliable (no branch row by design; the `branch_heads` row, if
/// present at all, may be from a prior orphaned pull and lag the real
/// remote HEAD).
///
/// The synthesized local commit copies `parent_hash` and
/// `merge_parent_hash` from the server's commit (via `/commits/info`) so
/// the LCA finder can walk back through the chain on later merges/syncs
/// — without that linkage, every multi-merge cycle drops the LCA to an
/// empty manifest and surfaces a full-file conflict on every changed
/// path.
///
/// Cost: one HTTP call to resolve the head, one to fetch commit
/// metadata, plus one tree-listing call per directory under it, plus
/// one raw-blob fetch for every file whose hash isn't already in our
/// blobs table. Repos that have been clone / pull'd recently will hit
/// the blob fast-path for most files.
pub(crate) async fn fetch_parent_from_server(
    repo: &dyn Repository,
    parent_name: &str,
) -> Result<Option<Hash>> {
    // We only know how to fetch `main` automatically — that's the parent
    // that doesn't get pushed under the normal personal-branch model.
    // For any other parent, surface a clear "ran out of options" error
    // rather than silently no-op'ing into an empty manifest.
    if parent_name != "main" {
        return Err(OakError::BranchNotFound(format!(
            "Parent branch '{parent_name}' isn't materialized locally and isn't 'main'. \
             Run `oak pull --branch {parent_name}` first."
        )));
    }

    let remote = repo.get_metadata(MetadataKey::RemoteUrl)?.ok_or_else(|| {
        OakError::Server("Repository has no remote configured. Run `oak push` first.".into())
    })?;
    let remote = remote.trim_end_matches('/').to_string();
    let (owner, repo_name) = super::read_repo_identity(repo)?;
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(&remote));

    let client = crate::http::api_client();

    output::info(&format!("Updating '{parent_name}' from {remote}…"));

    // 1. Resolve parent's HEAD via the repo info endpoint. Non-success goes
    // through `server_error` rather than `error_for_status`: the latter
    // passes 3xx through (redirects are disabled on `api_client`), so a
    // moved host would surface as a JSON decode error instead of the
    // structured `RemoteMoved` that `oak fetch` / `oak pull` know how to
    // follow.
    let info_url = format!("{remote}/api/{owner}/{repo_name}");
    let resp = with_auth(client.get(&info_url), api_key.as_deref())
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    let info: RepoInfoResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    // No HEAD means the parent branch (`main`) hasn't been bootstrapped by
    // any squash-merge yet. That's a legitimate state — a repo where peers
    // have only pushed feature branches — not an error. Report "no parent
    // head" so the caller can treat the sync as a no-op.
    let Some(head_str) = info.head else {
        record_parent_checked(repo, parent_name);
        return Ok(None);
    };
    let head_hash = Hash(head_str.clone());

    // If we already have this commit, the local branch_heads row already
    // points at it, AND the manifest's blobs are all present, nothing to
    // fetch. The hydration check matters because older binaries could publish
    // `main`'s commit/manifest before every blob was local; trusting only the
    // commit row leaves the next sync/reset to fail with a bare "Blob not
    // found" during materialization.
    let local_head = repo.get_branch_head(parent_name)?;
    if local_head.as_ref() == Some(&head_hash) && parent_snapshot_is_hydrated(repo, &head_hash)? {
        reconcile_synthetic_main(repo, &head_hash)?;
        ensure_branch_row(repo, parent_name)?;
        record_parent_checked(repo, parent_name);
        return Ok(Some(head_hash));
    }

    // 2. Fetch the head commit's metadata first. Current servers ship the
    //    commit's manifest hash plus its tree objects with `/commits/info`,
    //    which lets us reconstruct the manifest with file modes intact and —
    //    critically for Invariant 1 — store it under the server's manifest
    //    hash verbatim instead of a locally recomputed one.
    let meta = fetch_commit_metadata(
        &client,
        &remote,
        &owner,
        &repo_name,
        &head_hash,
        api_key.as_deref(),
    )
    .await;

    // 3. Resolve the head's manifest entries. Canonical path: rebuild from
    //    the server's tree objects. Fallback (older server without
    //    `/commits/info` trees): walk the `tree` endpoint, which loses the
    //    executable bit and recomputes the root hash locally.
    let canonical: Option<(Hash, Vec<ManifestEntry>)> = meta
        .manifest_hash
        .as_ref()
        .and_then(|mh| entries_from_trees(mh, &meta.trees).map(|entries| (mh.clone(), entries)));
    let (head_manifest_hash, entries) = match &canonical {
        Some((mh, entries)) => (mh.clone(), entries.clone()),
        None => {
            let entries = walk_remote_tree(
                &client,
                &remote,
                &owner,
                &repo_name,
                &head_str,
                api_key.as_deref(),
            )
            .await?;
            // `Manifest::new` recomputes the root hash from the entries;
            // hashing is deterministic and content-addressed, so for repos
            // without executable/symlink entries this matches the server's
            // manifest hash for the commit.
            (Manifest::new(entries.clone()).hash, entries)
        }
    };

    // 4. Fetch any blob we don't already have.
    let mut fetched = 0usize;
    for entry in &entries {
        if !repo.has_blob(&entry.blob_hash)? {
            let raw_url = format!(
                "{remote}/api/{owner}/{repo_name}/raw/{head_str}/{}",
                entry.path
            );
            let resp = with_auth(client.get(&raw_url), api_key.as_deref())
                .send()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
            if !resp.status().is_success() {
                // Same rationale as the info GET above: `server_error` (not
                // `error_for_status`) so a moved host is reported as such.
                return Err(crate::http::server_error(resp).await);
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
            let blob = Blob::new(bytes.to_vec());
            // Sanity check: the hash we computed locally has to match the
            // hash the server told us about. Mismatch implies content
            // corruption in flight or a server bug — bail rather than
            // silently storing garbage under the wrong key.
            if blob.hash != entry.blob_hash {
                return Err(OakError::Server(format!(
                    "Blob hash mismatch for {}: server said {}, local computed {}",
                    entry.path, entry.blob_hash, blob.hash
                )));
            }
            repo.store_blob(&blob)?;
            fetched += 1;
        }
    }
    if fetched > 0 {
        output::info(&format!("  downloaded {fetched} new file(s)"));
    }

    // 5. Store the manifest. On the canonical path the server's own tree
    //    objects are stored, so `get_manifest(head_manifest_hash)` resolves
    //    the exact tree the server has (modes included). On the fallback
    //    path the tree is rebuilt from the walked entries.
    match &canonical {
        Some(_) => {
            for tree in &meta.trees {
                repo.store_tree(tree)?;
            }
        }
        None => repo.store_manifest(&Manifest::new(entries))?,
    }

    // 6. Synthesize a local commit row for the parent's HEAD. `hash` is
    //    copied verbatim from the server — that's the commit's
    //    authoritative identity. `author` / `message` / `timestamp` are
    //    all inputs to that hash (see `Commit::with_timestamp`), so we
    //    fill them from the server's response; otherwise the local row
    //    would carry metadata inconsistent with its own hash and would
    //    surface wrong values in `oak log` and `oak export`. If the
    //    metadata fetch failed (older server, network error) we fall back
    //    to placeholders so the sync still completes.
    let head_parent = meta.parent_hash.clone();
    let files = if meta.files.is_empty() {
        // Best effort only: fetch paths prefer server-provided file rows.
        // The manifest diff can only succeed when both sides are already
        // local; otherwise it returns empty rather than inventing changes.
        files_for_remote_commit(repo, head_parent.as_ref(), &head_manifest_hash)?
    } else {
        meta.files
    };
    let commit = Commit {
        hash: head_hash.clone(),
        branch_name: parent_name.to_string(),
        parent_hash: meta.parent_hash,
        merge_parent_hash: meta.merge_parent_hash,
        manifest_hash: head_manifest_hash,
        author: meta.author.unwrap_or_else(|| "<remote>".to_string()),
        message: meta.message,
        timestamp: meta.timestamp.unwrap_or_else(chrono::Utc::now),
        files,
    };
    repo.store_commit(&commit)?;

    // 6b. Backfill the parent commit chain.
    //
    // Step 6 synthesizes *only* the parent's head. Without its ancestors the
    // local `main` history is a single disconnected node, so the LCA finder
    // (which walks `parent_hash` pointers via `repo.get_commit`) can't bridge
    // from main's head back to a feature branch's fork point — the walk dies
    // at the first missing commit. The LCA then collapses to an empty
    // manifest and every changed file becomes a whole-file conflict (the
    // reported "37 conflicts on a clean tree" bug). Walk down `parent_hash`,
    // synthesizing a faithful row per ancestor, until we reach a commit
    // already stored locally (typically the branch's fork point, whose
    // manifest the LCA finder then returns) or the chain ends.
    //
    // Rows are content-addressed: each synthesized row keeps the server's
    // hash and is filled from the server's `parent_hash` / `manifest_hash` /
    // author / message / timestamp so it stays consistent with its own hash.
    // We do *not* fetch each ancestor's manifest object — the LCA finder only
    // loads the manifest of the LCA commit itself (already stored as the fork
    // point), and intermediate rows are needed only for ancestry traversal.
    //
    // `MAX_CHAIN_BACKFILL` bounds the walk against a pathological history; a
    // metadata-fetch miss (older server, network error, or a commit whose
    // `manifest_hash` the server didn't return) stops the walk rather than
    // storing an inconsistent row — strictly no worse than the pre-fix state.
    const MAX_CHAIN_BACKFILL: usize = 100_000;
    let mut next = head_parent;
    let mut backfilled = 0usize;
    while let Some(ancestor_hash) = next {
        if backfilled >= MAX_CHAIN_BACKFILL {
            break;
        }
        // Reaching history we already have means the chain is connected from
        // here on — its ancestors were stored by an earlier clone/pull.
        if repo.get_commit(&ancestor_hash)?.is_some() {
            break;
        }
        let ameta = fetch_commit_metadata(
            &client,
            &remote,
            &owner,
            &repo_name,
            &ancestor_hash,
            api_key.as_deref(),
        )
        .await;
        // Without the server's manifest_hash we can't store a faithful row
        // (the commit's hash derives from it); stop rather than corrupt the
        // chain with a placeholder.
        let Some(ancestor_manifest_hash) = ameta.manifest_hash else {
            break;
        };
        let ancestor_parent = ameta.parent_hash.clone();
        let files = if ameta.files.is_empty() {
            // Ancestor backfill does not fetch ancestor tree objects, so this
            // is only a conservative local-history fallback.
            files_for_remote_commit(repo, ancestor_parent.as_ref(), &ancestor_manifest_hash)?
        } else {
            ameta.files
        };
        let ancestor_commit = Commit {
            hash: ancestor_hash.clone(),
            branch_name: parent_name.to_string(),
            parent_hash: ameta.parent_hash,
            merge_parent_hash: ameta.merge_parent_hash,
            manifest_hash: ancestor_manifest_hash,
            author: ameta.author.unwrap_or_else(|| "<remote>".to_string()),
            message: ameta.message,
            timestamp: ameta.timestamp.unwrap_or_else(chrono::Utc::now),
            files,
        };
        repo.store_commit(&ancestor_commit)?;
        backfilled += 1;
        next = ancestor_parent;
    }
    if backfilled > 0 {
        output::info(&format!(
            "  linked {backfilled} ancestor commit(s) of '{parent_name}'"
        ));
    }

    // 6c. Reconcile swap (Invariant 1): a local synthetic `main` commit
    //     whose manifest equals this canonical head's is the same snapshot
    //     under a non-canonical identity. Repoint anything still anchored
    //     on it (branch heads, HEAD) to the canonical hash. The synthetic
    //     rows themselves are kept — commits are never deleted.
    reconcile_synthetic_main(repo, &head_hash)?;

    // 7. Branch row + head pointer.
    ensure_branch_row(repo, parent_name)?;
    repo.set_branch_head(parent_name, &head_hash)?;
    record_parent_checked(repo, parent_name);

    Ok(Some(head_hash))
}

fn parent_snapshot_is_hydrated(repo: &dyn Repository, head: &Hash) -> Result<bool> {
    let Some(commit) = repo.get_commit(head)? else {
        return Ok(false);
    };
    let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? else {
        return Ok(false);
    };
    for entry in &manifest.entries {
        if !repo.has_blob(&entry.blob_hash)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Flatten a commit's tree objects (as shipped by `/commits/info`) into flat
/// manifest entries. Returns `None` when the root or any referenced subtree
/// is missing from `trees` — the caller then falls back to walking the remote
/// `tree` endpoint.
fn entries_from_trees(root: &Hash, trees: &[oak_core::Tree]) -> Option<Vec<ManifestEntry>> {
    use std::collections::HashMap;
    if *root == oak_core::Tree::empty_hash() {
        return Some(Vec::new());
    }
    let by_hash: HashMap<&str, &oak_core::Tree> =
        trees.iter().map(|t| (t.hash.as_str(), t)).collect();
    let mut out = Vec::new();
    let mut stack: Vec<(String, &oak_core::Tree)> =
        vec![(String::new(), by_hash.get(root.as_str()).copied()?)];
    while let Some((prefix, tree)) = stack.pop() {
        for e in &tree.entries {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            match e.kind {
                oak_core::TreeEntryKind::Tree => {
                    stack.push((path, by_hash.get(e.hash.as_str()).copied()?))
                }
                oak_core::TreeEntryKind::Blob => out.push(ManifestEntry {
                    path,
                    blob_hash: e.hash.clone(),
                    mode: e.mode,
                }),
            }
        }
    }
    Some(out)
}

/// Invariant 1's reconcile pass. Local `main` may only ever point at
/// server-minted commit hashes — but older binaries (and ingestion paths that
/// fell back to tree walks) sometimes synthesized a local `main` commit row
/// for the same snapshot under a different hash. Those duplicates are
/// detected by content: a local `main` commit whose manifest hash equals the
/// canonical head's. Every branch head (and HEAD) still pointing at such a
/// duplicate is repointed to the canonical hash — content-addressed equality
/// makes the swap safe and silent. The duplicate rows are kept: commits are
/// never deleted.
///
/// Returns the names of the branches whose heads were repointed.
pub(crate) fn reconcile_synthetic_main(
    repo: &dyn Repository,
    canonical: &Hash,
) -> Result<Vec<String>> {
    let Some(canon) = repo.get_commit(canonical)? else {
        return Ok(Vec::new());
    };
    let synthetic: std::collections::HashSet<String> = repo
        .get_commits_for_branch(oak_core::DEFAULT_BRANCH)?
        .into_iter()
        .filter(|c| c.hash != *canonical && c.manifest_hash == canon.manifest_hash)
        .map(|c| c.hash.0)
        .collect();
    if synthetic.is_empty() {
        return Ok(Vec::new());
    }
    let mut repointed = Vec::new();
    for br in repo.list_branches()? {
        if let Some(head) = repo.get_branch_head(&br.name)? {
            if synthetic.contains(head.as_str()) {
                repo.set_branch_head(&br.name, canonical)?;
                repointed.push(br.name);
            }
        }
    }
    if let Some(h) = repo.get_head()? {
        if synthetic.contains(h.as_str()) {
            repo.set_head(canonical)?;
        }
    }
    if !repointed.is_empty() {
        output::vlog(&format!(
            "reconciled {} branch head(s) from synthetic main commit(s) onto {}",
            repointed.len(),
            canonical.short()
        ));
    }
    Ok(repointed)
}

fn record_parent_checked(repo: &dyn Repository, parent_name: &str) {
    if parent_name != oak_core::DEFAULT_BRANCH {
        return;
    }
    let checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
        .to_string();
    let _ = repo.set_metadata(MetadataKey::MainLastCheckedAt, &checked_at);
}

fn ensure_branch_row(repo: &dyn Repository, name: &str) -> Result<()> {
    if repo.get_branch(name)?.is_none() {
        let branch = oak_core::Branch::new(name.to_string(), None, None);
        repo.store_branch(&branch)?;
    }
    Ok(())
}

/// Rebuild display-only file-change metadata for a commit whose authoritative
/// row came from the server.
///
/// The server hash is already fixed; these file rows are not part of the
/// current v1 hash preimage. They keep `oak log`, branch review, and agents'
/// history inspection honest when a remote commit can be compared against
/// local manifests, such as a just-landed merge.
///
/// This is a conservative best-effort fallback. Fetch/backfill paths should
/// prefer server-provided file rows because ancestor manifests are often not
/// hydrated locally. If either side is missing, return an empty list rather
/// than pretending the whole tree changed.
pub(crate) fn files_for_remote_commit(
    repo: &dyn Repository,
    parent_hash: Option<&Hash>,
    manifest_hash: &Hash,
) -> Result<Vec<FileChange>> {
    let Some(new_manifest) = repo.get_manifest(manifest_hash)? else {
        return Ok(Vec::new());
    };
    let old_manifest = match parent_hash {
        Some(parent_hash) => {
            let Some(parent_commit) = repo.get_commit(parent_hash)? else {
                return Ok(Vec::new());
            };
            let Some(parent_manifest) = repo.get_manifest(&parent_commit.manifest_hash)? else {
                return Ok(Vec::new());
            };
            parent_manifest
        }
        None => Manifest::empty(),
    };
    Ok(old_manifest.diff(&new_manifest))
}

#[derive(Serialize)]
struct CommitInfoReq<'a> {
    hashes: Vec<&'a str>,
}

#[derive(Deserialize)]
struct CommitInfoResp {
    #[serde(default)]
    commits: Vec<CommitInfoEntry>,
    /// Tree objects reachable from the returned commits' root trees.
    /// Present on current servers; older servers omit the field.
    #[serde(default)]
    trees: Vec<oak_core::protocol::TreeData>,
}

#[derive(Deserialize)]
struct CommitInfoEntry {
    hash: String,
    #[serde(default)]
    parent_hash: Option<String>,
    #[serde(default)]
    merge_parent_hash: Option<String>,
    #[serde(default)]
    manifest_hash: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    files: Vec<oak_core::protocol::FileChangeData>,
}

/// Subset of a commit's server-side metadata used to synthesize a
/// faithful local row for a `main` commit we fetched but didn't author.
/// Every field is optional so a metadata-fetch failure degrades to
/// placeholders at the call site rather than erroring.
#[derive(Default)]
struct RemoteCommitMeta {
    parent_hash: Option<Hash>,
    merge_parent_hash: Option<Hash>,
    /// The commit's manifest hash. Needed to synthesize a faithful local row
    /// for an ancestor we backfill (the commit's content hash derives from
    /// it). `None` if the server didn't return it or the fetch failed.
    manifest_hash: Option<Hash>,
    author: Option<String>,
    message: Option<String>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    files: Vec<FileChange>,
    /// Tree objects for the commit, when the server shipped them with
    /// `/commits/info`. Storing these (instead of re-walking the `tree`
    /// endpoint) preserves file modes and — critically — makes the local
    /// manifest hash equal the server's verbatim, so commit identity stays
    /// canonical (Invariant 1).
    trees: Vec<oak_core::Tree>,
}

/// Best-effort fetch of a single commit's metadata from the server.
/// Returns `parent_hash` / `merge_parent_hash` (needed by the LCA finder)
/// plus `author` / `message` / `timestamp` — all of which are inputs to
/// the commit content hash (see `Commit::with_timestamp`), so the caller
/// needs them to synthesize a local row that matches the commit it claims
/// to be. Returns `RemoteCommitMeta::default()` (all `None`) on any
/// failure; the caller then falls back to placeholders. Older servers
/// without `/commits/info` just hit the error branch and degrade
/// gracefully.
async fn fetch_commit_metadata(
    client: &reqwest::Client,
    remote: &str,
    owner: &str,
    repo_name: &str,
    head: &Hash,
    api_key: Option<&str>,
) -> RemoteCommitMeta {
    let url = format!("{remote}/api/{owner}/{repo_name}/commits/info");
    let body = CommitInfoReq {
        hashes: vec![head.0.as_str()],
    };
    let resp = match with_auth(client.post(&url).json(&body), api_key)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return RemoteCommitMeta::default(),
    };
    let parsed: CommitInfoResp = match resp.json().await {
        Ok(p) => p,
        Err(_) => return RemoteCommitMeta::default(),
    };
    let trees: Vec<oak_core::Tree> = parsed
        .trees
        .iter()
        .filter_map(|td| oak_core::protocol::tree_data_to_core(td).ok())
        .collect();
    let entry = match parsed.commits.into_iter().find(|c| c.hash == head.0) {
        Some(e) => e,
        None => return RemoteCommitMeta::default(),
    };
    let files = entry.files.into_iter().map(file_change_from_wire).collect();
    RemoteCommitMeta {
        parent_hash: entry.parent_hash.map(Hash),
        merge_parent_hash: entry.merge_parent_hash.map(Hash),
        manifest_hash: entry.manifest_hash.map(Hash),
        author: entry.author,
        message: entry.message,
        timestamp: entry
            .timestamp
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc)),
        files,
        trees,
    }
}

fn file_change_from_wire(file: oak_core::protocol::FileChangeData) -> FileChange {
    FileChange {
        path: file.path,
        change_type: match file.change_type.as_str() {
            "deleted" => oak_core::ChangeType::Deleted,
            "renamed" => oak_core::ChangeType::Renamed,
            "modified" => oak_core::ChangeType::Modified,
            _ => oak_core::ChangeType::Added,
        },
        old_blob_hash: file.old_blob_hash.map(Hash),
        new_blob_hash: file.new_blob_hash.map(Hash),
        old_path: file.old_path,
        old_mode: None,
        new_mode: None,
    }
}

fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(key) = api_key {
        builder.header("authorization", format!("Bearer {key}"))
    } else {
        builder
    }
}

/// Recursively walk the `tree` endpoint for `commit`, returning a flat
/// list of (path, blob_hash, mode) entries. Mode is reconstructed from
/// the path (the tree endpoint doesn't surface the executable bit today,
/// so non-executable Regular is used — this is acceptable because
/// downstream consumers just need the hash to do a 3-way merge).
async fn walk_remote_tree(
    client: &reqwest::Client,
    remote: &str,
    owner: &str,
    repo_name: &str,
    commit: &str,
    api_key: Option<&str>,
) -> Result<Vec<ManifestEntry>> {
    let mut out = Vec::new();
    // (server-side path, "" at the root)
    let mut stack: Vec<String> = vec![String::new()];
    while let Some(dir) = stack.pop() {
        // Root listing hits the no-path route — the server's nested
        // `tree/{commit}/{*path}` route can't match an empty wildcard, so
        // a trailing slash here would 404. Subdirectories take the
        // nested route as usual.
        let url = if dir.is_empty() {
            format!("{remote}/api/{owner}/{repo_name}/tree/{commit}")
        } else {
            format!("{remote}/api/{owner}/{repo_name}/tree/{commit}/{dir}")
        };
        let resp = with_auth(client.get(&url), api_key)
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(OakError::Server(format!(
                "tree listing failed for '{dir}': {}",
                crate::http::error_text(resp).await
            )));
        }
        let entries: Vec<TreeEntryResponse> = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        for e in entries {
            match e.kind.as_str() {
                "tree" => stack.push(e.path),
                "blob" => out.push(ManifestEntry {
                    path: e.path,
                    blob_hash: Hash(e.blob_hash),
                    mode: FileMode::Regular,
                }),
                other => {
                    return Err(OakError::Server(format!(
                        "unknown tree entry kind '{other}' for {}",
                        e.name
                    )));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod canonical_main_tests {
    use super::*;
    use oak_core::{FileMode, Repository, SqliteRepository};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as urlpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn open_linked_repo(temp: &TempDir, remote: &str) -> SqliteRepository {
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "oak").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "oak").unwrap();
        repo
    }

    /// Real content-addressed tree fixture: root hash + wire-shaped trees.
    fn tree_fixture(
        repo: &SqliteRepository,
        entries: Vec<ManifestEntry>,
    ) -> (Hash, Vec<serde_json::Value>) {
        let root = repo.put_manifest(entries).unwrap();
        let mut fetch = |h: &Hash| -> Result<oak_core::Tree> {
            repo.get_tree(h)?
                .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
        };
        let trees = oak_core::collect_tree_objects(&root, &mut fetch).unwrap();
        let wire = trees
            .iter()
            .map(|t| serde_json::to_value(oak_core::protocol::tree_to_wire(t)).unwrap())
            .collect();
        (root, wire)
    }

    /// Invariant 1, canonical path: with `/commits/info` shipping trees, the
    /// synthesized main row carries the server's commit AND manifest hashes
    /// verbatim — and file modes survive (the tree-walk fallback loses them).
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_parent_stores_server_hashes_verbatim_via_commit_info_trees() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let repo = open_linked_repo(&temp, &server.uri());

        let content = b"#!/bin/sh\necho hi\n";
        let blob_hash = oak_core::hash_bytes(content);
        // Build the fixture in a scratch repo so the local repo starts
        // without the blob/trees (forcing the full fetch path).
        let scratch_dir = TempDir::new().unwrap();
        let scratch = SqliteRepository::open(&scratch_dir.path().join("s.db")).unwrap();
        let (root, wire_trees) = tree_fixture(
            &scratch,
            vec![ManifestEntry {
                path: "tool.sh".to_string(),
                blob_hash: blob_hash.clone(),
                mode: FileMode::Executable,
            }],
        );
        let head: String = "ab".repeat(32);
        let parent: String = "cd".repeat(32);

        Mock::given(method("GET"))
            .and(urlpath("/api/oak/oak"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": head
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [{
                    "hash": head,
                    "branch_name": "main",
                    "parent_hash": parent.clone(),
                    "manifest_hash": root.to_string(),
                    "author": "alice",
                    "message": "squash",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "files": [{
                        "path": "tool.sh",
                        "change_type": "added",
                        "old_blob_hash": null,
                        "new_blob_hash": blob_hash.to_string()
                    }]
                }],
                "trees": wire_trees
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(urlpath(format!("/api/oak/oak/raw/{head}/tool.sh")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&server)
            .await;

        let got = fetch_parent_from_server(&repo, "main").await.unwrap();
        assert_eq!(got.as_ref().map(|h| h.to_string()), Some(head.clone()));

        let commit = repo.get_commit(&Hash(head.clone())).unwrap().unwrap();
        assert_eq!(
            commit.manifest_hash, root,
            "manifest hash must be the server's, verbatim"
        );
        assert_eq!(commit.author, "alice");
        assert_eq!(commit.message.as_deref(), Some("squash"));
        assert_eq!(
            commit.parent_hash.as_ref().map(|h| h.to_string()),
            Some(parent)
        );
        assert_eq!(commit.files.len(), 1);
        assert_eq!(commit.files[0].path, "tool.sh");
        assert_eq!(commit.files[0].change_type, oak_core::ChangeType::Added);
        let manifest = repo.get_manifest(&root).unwrap().unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(
            manifest.entries[0].mode,
            FileMode::Executable,
            "modes must survive the canonical ingestion path"
        );
        assert_eq!(
            repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
            Some(head)
        );
    }

    /// A previously interrupted parent refresh can leave `main` with the head
    /// commit and manifest present but some manifest blobs absent. Do not
    /// trust that as "already fetched"; hydrate the missing blobs before sync
    /// tries to merge/materialize from `main`.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_parent_repairs_existing_head_with_missing_manifest_blobs() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let repo = open_linked_repo(&temp, &server.uri());

        let content = b"tree module\n";
        let blob_hash = oak_core::hash_bytes(content);
        let (root, wire_trees) = tree_fixture(
            &repo,
            vec![ManifestEntry {
                path: "core/src/tree.rs".to_string(),
                blob_hash: blob_hash.clone(),
                mode: FileMode::Regular,
            }],
        );
        let head: String = "dd".repeat(32);
        repo.store_commit(&Commit {
            hash: Hash(head.clone()),
            branch_name: "main".to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: root.clone(),
            author: "<remote>".to_string(),
            message: Some("main head".to_string()),
            timestamp: chrono::Utc::now(),
            files: Vec::new(),
        })
        .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &Hash(head.clone())).unwrap();
        assert!(
            !repo.has_blob(&blob_hash).unwrap(),
            "fixture must start with a torn local main snapshot"
        );

        Mock::given(method("GET"))
            .and(urlpath("/api/oak/oak"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": head
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [{
                    "hash": head,
                    "branch_name": "main",
                    "parent_hash": null,
                    "manifest_hash": root.to_string(),
                    "author": "<remote>",
                    "message": "main head",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "files": []
                }],
                "trees": wire_trees
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(urlpath(format!("/api/oak/oak/raw/{head}/core/src/tree.rs")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let got = fetch_parent_from_server(&repo, "main").await.unwrap();

        assert_eq!(got.map(|h| h.to_string()), Some(head.clone()));
        assert!(
            repo.has_blob(&blob_hash).unwrap(),
            "missing parent blob must be repaired from the remote"
        );
        assert_eq!(
            repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
            Some(head)
        );
    }

    /// Invariant 1, fallback path (older server without `/commits/info`):
    /// the tree-walk reconstruction still stores the server's COMMIT hash
    /// verbatim — never a locally minted one.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_parent_fallback_tree_walk_keeps_server_commit_hash() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let repo = open_linked_repo(&temp, &server.uri());

        let content = b"readme\n";
        let blob_hash = oak_core::hash_bytes(content);
        let head: String = "cd".repeat(32);

        Mock::given(method("GET"))
            .and(urlpath("/api/oak/oak"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": head
            })))
            .mount(&server)
            .await;
        // No /commits/info mock: wiremock answers 404, like an older server.
        Mock::given(method("GET"))
            .and(urlpath(format!("/api/oak/oak/tree/{head}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "name": "README.md",
                    "path": "README.md",
                    "kind": "blob",
                    "blob_hash": blob_hash.to_string()
                }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(urlpath(format!("/api/oak/oak/raw/{head}/README.md")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&server)
            .await;

        let got = fetch_parent_from_server(&repo, "main").await.unwrap();
        assert_eq!(got.as_ref().map(|h| h.to_string()), Some(head.clone()));
        let commit = repo.get_commit(&Hash(head.clone())).unwrap().unwrap();
        assert_eq!(commit.branch_name, "main");
        assert!(repo.get_manifest(&commit.manifest_hash).unwrap().is_some());
        assert_eq!(
            repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
            Some(head)
        );
    }

    #[test]
    fn files_for_remote_commit_diffs_when_parent_and_child_manifests_are_local() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let readme_blob = repo.put_blob(b"base\n".to_vec()).unwrap();
        let readme_entry = ManifestEntry {
            path: "README.md".to_string(),
            blob_hash: readme_blob,
            mode: FileMode::Regular,
        };
        let parent_manifest = repo.put_manifest(vec![readme_entry.clone()]).unwrap();
        let parent = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                parent_manifest,
                "tester".to_string(),
                Some("base".to_string()),
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();

        let feature_blob = repo.put_blob(b"feature\n".to_vec()).unwrap();
        let feature_entry = ManifestEntry {
            path: "feature.txt".to_string(),
            blob_hash: feature_blob.clone(),
            mode: FileMode::Regular,
        };
        let child_manifest = repo
            .put_manifest(vec![readme_entry, feature_entry])
            .unwrap();

        let files = files_for_remote_commit(&repo, Some(&parent), &child_manifest).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "feature.txt");
        assert_eq!(files[0].change_type, oak_core::ChangeType::Added);
        assert_eq!(files[0].old_blob_hash, None);
        assert_eq!(files[0].new_blob_hash, Some(feature_blob));
    }

    #[test]
    fn files_for_remote_commit_returns_empty_when_parent_is_not_local() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let blob = repo.put_blob(b"content\n".to_vec()).unwrap();
        let manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "file.txt".to_string(),
                blob_hash: blob,
                mode: FileMode::Regular,
            }])
            .unwrap();
        let missing_parent = Hash("12".repeat(32));

        let files = files_for_remote_commit(&repo, Some(&missing_parent), &manifest).unwrap();
        assert!(
            files.is_empty(),
            "without the parent manifest, do not invent a whole-tree diff"
        );
    }

    /// The reconcile swap, pure-local: branch heads sitting on a synthetic
    /// main commit (same manifest, different hash) are repointed to the
    /// canonical hash; the synthetic row itself is kept.
    #[test]
    fn reconcile_synthetic_main_repoints_content_equal_heads() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let blob = repo.put_blob(b"hello\n".to_vec()).unwrap();
        let manifest_hash = repo
            .put_manifest(vec![ManifestEntry {
                path: "hello.txt".to_string(),
                blob_hash: blob,
                mode: FileMode::Regular,
            }])
            .unwrap();
        // Synthetic: locally minted hash for main's snapshot.
        let synthetic = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                manifest_hash.clone(),
                "local".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        // Canonical: the server's row for the same snapshot.
        let canonical = Hash("ef".repeat(32));
        repo.store_commit(&Commit {
            hash: canonical.clone(),
            branch_name: "main".to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash,
            author: "<remote>".to_string(),
            message: None,
            timestamp: chrono::Utc::now(),
            files: Vec::new(),
        })
        .unwrap();

        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &synthetic).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &synthetic).unwrap();
        repo.set_head(&synthetic).unwrap();

        let mut repointed = reconcile_synthetic_main(&repo, &canonical).unwrap();
        repointed.sort();
        assert_eq!(repointed, vec!["feature".to_string(), "main".to_string()]);
        assert_eq!(
            repo.get_branch_head("main").unwrap(),
            Some(canonical.clone())
        );
        assert_eq!(
            repo.get_branch_head("feature").unwrap(),
            Some(canonical.clone())
        );
        assert_eq!(repo.get_head().unwrap(), Some(canonical));
        assert!(
            repo.get_commit(&synthetic).unwrap().is_some(),
            "synthetic rows are kept — commits are never deleted"
        );
    }

    /// A branch with its own (different-manifest) head is never touched by
    /// the swap.
    #[test]
    fn reconcile_synthetic_main_leaves_real_branch_work_alone() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();

        let blob = repo.put_blob(b"work\n".to_vec()).unwrap();
        let work_manifest = repo
            .put_manifest(vec![ManifestEntry {
                path: "work.txt".to_string(),
                blob_hash: blob,
                mode: FileMode::Regular,
            }])
            .unwrap();
        let work_tip = repo
            .put_commit(
                "feature".to_string(),
                None,
                None,
                work_manifest,
                "dev".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "feature".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("feature", &work_tip).unwrap();

        let canonical = Hash("ef".repeat(32));
        let empty_manifest = repo.put_manifest(Vec::new()).unwrap();
        repo.store_commit(&Commit {
            hash: canonical.clone(),
            branch_name: "main".to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: empty_manifest,
            author: "<remote>".to_string(),
            message: None,
            timestamp: chrono::Utc::now(),
            files: Vec::new(),
        })
        .unwrap();

        let repointed = reconcile_synthetic_main(&repo, &canonical).unwrap();
        assert!(repointed.is_empty());
        assert_eq!(repo.get_branch_head("feature").unwrap(), Some(work_tip));
    }

    /// End-to-end swap through `fetch_parent_from_server`: a branch seeded
    /// at a synthetic local main lands on the canonical hash once the
    /// server's head (same content) is fetched.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_parent_repoints_branches_off_synthetic_main() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let repo = open_linked_repo(&temp, &server.uri());

        let blob_content = b"shared\n";
        let blob = repo.put_blob(blob_content.to_vec()).unwrap();
        let (root, wire_trees) = tree_fixture(
            &repo,
            vec![ManifestEntry {
                path: "shared.txt".to_string(),
                blob_hash: blob,
                mode: FileMode::Regular,
            }],
        );
        // Synthetic local identity for main's snapshot, with a branch on it.
        let synthetic = repo
            .put_commit(
                "main".to_string(),
                None,
                None,
                root.clone(),
                "local".to_string(),
                None,
                chrono::Utc::now(),
                Vec::new(),
            )
            .unwrap();
        repo.store_branch(&oak_core::Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_branch_head("main", &synthetic).unwrap();
        repo.store_branch(&oak_core::Branch::new(
            "fresh".to_string(),
            None,
            Some("main".to_string()),
        ))
        .unwrap();
        repo.set_branch_head("fresh", &synthetic).unwrap();

        let canonical: String = "fa".repeat(32);
        Mock::given(method("GET"))
            .and(urlpath("/api/oak/oak"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": canonical
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [{
                    "hash": canonical,
                    "branch_name": "main",
                    "parent_hash": null,
                    "manifest_hash": root.to_string(),
                    "author": "<remote>",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "files": []
                }],
                "trees": wire_trees
            })))
            .mount(&server)
            .await;

        let got = fetch_parent_from_server(&repo, "main").await.unwrap();
        assert_eq!(got.map(|h| h.to_string()), Some(canonical.clone()));
        assert_eq!(
            repo.get_branch_head("main").unwrap().map(|h| h.to_string()),
            Some(canonical.clone()),
            "main repointed to the canonical hash"
        );
        assert_eq!(
            repo.get_branch_head("fresh")
                .unwrap()
                .map(|h| h.to_string()),
            Some(canonical),
            "branch seeded at the synthetic main follows the swap"
        );
        assert!(
            repo.get_commit(&synthetic).unwrap().is_some(),
            "synthetic row kept"
        );
    }
}
