use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{
    three_way_merge_manifests, Blob, Commit, FileChange, FileMode, Hash, IgnorePatterns, Manifest,
    ManifestEntry, MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};

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
            let (changes, _, _) = super::commit::compute_changes(&repo, root)?;
            if let Some(reconciled) = super::merge::apply_remote_merge_reconcile(
                &lock,
                &repo,
                root,
                changes.is_empty(),
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
        super::merge::write_manifest_to_workdir(&lock, root, &repo, &merged_manifest)?;

        // Persist the merge result itself (the manifest object plus a
        // SYNC_STATE record of its hash and the conflicted paths). That's
        // what lets `sync_continue` commit exactly "3-way merge result +
        // the user's conflict resolutions" instead of snapshotting the
        // whole working tree — which used to silently sweep every
        // unrelated dirty file into the sync commit. Written before
        // SYNC_HEAD so a crash between the two leaves no SYNC_STATE-less
        // half-state behind (SYNC_HEAD is what `--continue` keys off).
        repo.store_manifest(&merged_manifest)?;
        let state = SyncState {
            merged_manifest_hash: merged_manifest.hash.0.clone(),
            conflict_paths: conflict_paths
                .iter()
                .chain(binary_conflict_paths.iter())
                .cloned()
                .collect(),
        };
        let state_json = serde_json::to_string_pretty(&state)
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
        fs::write(root.join(".oak/SYNC_STATE"), state_json)?;

        // Save sync state (using SYNC_HEAD to distinguish from merge).
        // The third line records the parent's HEAD at sync time so
        // `sync --continue` can stamp it as `merge_parent_hash` on the
        // resolved commit — that's what lets future merges of this branch
        // back into the parent find a non-trivial LCA. Older SYNC_HEAD
        // files (two lines) are still handled by `sync_continue` for
        // backwards compatibility; they just produce a sync commit with
        // merge_parent_hash = None.
        let sync_head_path = root.join(".oak/SYNC_HEAD");
        let sync_msg_path = root.join(".oak/SYNC_MSG");
        let parent_head_line = parent_head
            .as_ref()
            .map(|h| h.0.as_str())
            .unwrap_or_default();
        fs::write(
            &sync_head_path,
            format!("{parent_name}\n{branch_name}\n{parent_head_line}"),
        )?;
        fs::write(
            &sync_msg_path,
            format!("Sync branch '{branch_name}' from '{parent_name}'"),
        )?;

        output::warning(&format!(
            "Sync conflict: {total_conflicts} file(s) need manual resolution"
        ));
        for p in &conflict_paths {
            output::info(&format!("  CONFLICT (content): {p}"));
        }
        for p in &binary_conflict_paths {
            output::info(&format!("  CONFLICT (binary): {p} - kept branch version"));
        }
        output::info("");
        output::info("Fix the conflicts and then run 'oak pull --continue'");
        output::info("To abort the sync, run 'oak pull --abort'");

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

    let author = std::env::var("OAK_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());

    repo.store_manifest(merged_manifest)?;

    // Create sync commit on the CURRENT branch (not the parent). Sync
    // commits don't carry a message — feature-branch commits never do.
    // `merge_parent_hash = parent's HEAD at sync time` is what makes a
    // later merge of this branch into the parent find a recent LCA
    // (otherwise the LCA finder falls back to the original fork point
    // and any file modified on both sides since then becomes a conflict).
    let sync_commit = Commit::new(
        branch_name.to_string(),
        branch_head,
        merge_parent,
        merged_manifest.hash.clone(),
        author,
        None,
        file_changes,
    )?;

    repo.store_commit(&sync_commit)?;
    repo.set_branch_head(branch_name, &sync_commit.hash)?;
    repo.set_head(&sync_commit.hash)?;

    // Update working directory to synced state
    if update_workdir {
        crate::commands::reset::reset_to_manifest(lock, root, repo, merged_manifest)?;
    }

    output::success(&format!(
        "Synced branch '{branch_name}' from '{parent_name}'"
    ));
    output::info(&format!("  commit {}", sync_commit.hash.short()));

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

    complete_sync(
        &lock,
        &repo,
        root,
        &branch_name,
        &parent_name,
        branch_head,
        merge_parent,
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

    // If we already have this commit AND the local branch_heads row
    // already points at it, nothing to fetch. Otherwise normalize the
    // local pointer to the server's authoritative head.
    let local_head = repo.get_branch_head(parent_name)?;
    if repo.get_commit(&head_hash)?.is_some() && local_head.as_ref() == Some(&head_hash) {
        ensure_branch_row(repo, parent_name)?;
        record_parent_checked(repo, parent_name);
        return Ok(Some(head_hash));
    }

    // 2. Walk parent's tree to enumerate every (path, blob_hash) pair.
    let entries = walk_remote_tree(
        &client,
        &remote,
        &owner,
        &repo_name,
        &head_str,
        api_key.as_deref(),
    )
    .await?;

    // 3. Fetch any blob we don't already have.
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

    // 4. Build + store the manifest from the entries. `Manifest::new`
    //    recomputes the root hash from the entries; because hashing is
    //    deterministic and content-addressed, the result will match the
    //    server's manifest_hash for this commit.
    let manifest = Manifest::new(entries);
    repo.store_manifest(&manifest)?;

    // 5. Fetch the parent commit's metadata. The LCA finder walks the
    //    `parent_hash` / `merge_parent_hash` pointers, so without them a
    //    feature branch that's been merged once already loses its
    //    connection to the squash commit's history on the next sync —
    //    every changed file then becomes a whole-file conflict against an
    //    empty base manifest. We also pull `author` / `message` /
    //    `timestamp` so the synthesized row below is faithful to the real
    //    commit (see step 6).
    let meta = fetch_commit_metadata(
        &client,
        &remote,
        &owner,
        &repo_name,
        &head_hash,
        api_key.as_deref(),
    )
    .await;

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
    let commit = Commit {
        hash: head_hash.clone(),
        branch_name: parent_name.to_string(),
        parent_hash: meta.parent_hash,
        merge_parent_hash: meta.merge_parent_hash,
        manifest_hash: manifest.hash.clone(),
        author: meta.author.unwrap_or_else(|| "<remote>".to_string()),
        message: meta.message,
        timestamp: meta.timestamp.unwrap_or_else(chrono::Utc::now),
        files: Vec::new(),
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
        let ancestor_commit = Commit {
            hash: ancestor_hash.clone(),
            branch_name: parent_name.to_string(),
            parent_hash: ameta.parent_hash,
            merge_parent_hash: ameta.merge_parent_hash,
            manifest_hash: ancestor_manifest_hash,
            author: ameta.author.unwrap_or_else(|| "<remote>".to_string()),
            message: ameta.message,
            timestamp: ameta.timestamp.unwrap_or_else(chrono::Utc::now),
            files: Vec::new(),
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

    // 7. Branch row + head pointer.
    ensure_branch_row(repo, parent_name)?;
    repo.set_branch_head(parent_name, &head_hash)?;
    record_parent_checked(repo, parent_name);

    Ok(Some(head_hash))
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

#[derive(Serialize)]
struct CommitInfoReq<'a> {
    hashes: Vec<&'a str>,
}

#[derive(Deserialize)]
struct CommitInfoResp {
    #[serde(default)]
    commits: Vec<CommitInfoEntry>,
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
    let entry = match parsed.commits.into_iter().find(|c| c.hash == head.0) {
        Some(e) => e,
        None => return RemoteCommitMeta::default(),
    };
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
                    )))
                }
            }
        }
    }
    Ok(out)
}
