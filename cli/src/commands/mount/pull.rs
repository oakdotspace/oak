//! `oak pull` inside a mount — bring the parent branch (the trunk) into the
//! mount's virtual branch and resolve any conflicts, without leaving the mount.
//!
//! A mount's virtual branch is parented onto the trunk and forked at the
//! commit the mount was created on (`base_commit`). When the trunk moves ahead,
//! merging the virtual branch back into it can conflict. This command lets an
//! agent working in the mount pull those trunk changes in, resolve the
//! conflicts locally, and push — the same loop a regular `oak pull` runs, but
//! over the mount's cache db + overlay instead of a `.oak/` working tree.
//!
//! Design (kept lazy — the mount never has the trunk's full blob set):
//! - The full merged manifest (clean trunk changes folded in, conflict files
//!   pointing at marker blobs) is stored in the cache db and recorded in the
//!   mount sync state. Clean trunk changes ride along as manifest entries and
//!   are never written to disk — `oak push` skips blobs the server already has.
//! - Only the *conflicting* text files are materialized into the overlay (with
//!   markers) so the user has something to edit.
//! - `--continue` layers the user's resolved overlay files on top of that
//!   merged manifest and lands one sync commit (stamped with the trunk head as
//!   `merge_parent_hash`). `--abort` discards the overlay + sync state.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use chrono::Utc;
use oak_core::{Blob, FileChange, FileMode, Hash, Manifest, ManifestEntry, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use super::state::{
    cache_db_path, clear_sync_state, load_overlay_meta, load_sync_state, overlay_dir,
    save_overlay_meta, save_sync_state, MountConfig, MountSyncState, OverlayMeta,
};
use super::{build_committed_manifest, config_for_dest, split_overlay, token_for};
use crate::output;

/// Does `s` still contain unresolved conflict markers?
fn has_markers(s: &str) -> bool {
    crate::commands::merge::content_has_conflict_markers(s)
}

/// Entry point for `oak pull` when the cwd is inside a mount.
pub async fn run(
    dest: &Path,
    _force: bool,
    continue_after_conflict: bool,
    abort: bool,
) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;

    if continue_after_conflict && abort {
        return Err(OakError::InvalidArgument(
            "--continue and --abort are mutually exclusive".into(),
        ));
    }
    if abort {
        return abort_pull(&cfg, &state_dir);
    }
    if continue_after_conflict {
        return continue_pull(&cfg, &state_dir);
    }
    fresh_pull(&cfg, &state_dir).await
}

/// Start a pull: fetch the trunk, run the 3-way merge, and either land a clean
/// sync commit or stage the conflicts for the user to resolve.
async fn fresh_pull(cfg: &MountConfig, state_dir: &Path) -> Result<()> {
    if load_sync_state(state_dir)?.is_some() {
        return Err(OakError::MergeFailed(
            "a pull is already in progress — run `oak pull --continue` or `oak pull --abort`."
                .into(),
        ));
    }

    let overlay = load_overlay_meta(state_dir)?;
    if !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty() {
        return Err(OakError::Server(
            "uncommitted changes in the mount overlay — run `oak commit` (or discard them) before \
             `oak pull`."
                .into(),
        ));
    }

    let cache = SqliteRepository::open_relaxed(&cache_db_path(state_dir))?;
    let token = token_for(&cfg.remote_url);

    // The parent is recorded on the virtual branch row (always the trunk).
    let parent_name = cache
        .get_branch(&cfg.virtual_branch)?
        .and_then(|b| b.parent_branch)
        .unwrap_or_else(|| oak_core::DEFAULT_BRANCH.to_string());

    output::info(&format!(
        "Pulling '{parent_name}' into '{}' from {}...",
        cfg.virtual_branch, cfg.remote_url
    ));

    // Resolve the trunk's current head on the server and fetch its history into
    // the cache (down to the mount's fork point) so the merge-base walk works.
    let (_resolved_name, parent_head_str) = super::remote::resolve_branch_head(
        &cfg.remote_url,
        &cfg.owner,
        &cfg.repo,
        Some(&parent_name),
        token.as_deref(),
    )
    .await?;
    let parent_head = Hash(parent_head_str);
    fetch_parent_history(&cache, cfg, token.as_deref(), &parent_head).await?;

    let branch_head = cache
        .get_branch_head(&cfg.virtual_branch)?
        .ok_or_else(|| OakError::Server("virtual branch has no head".into()))?;
    let branch_manifest = manifest_for(&cache, &branch_head)?;
    let parent_manifest = manifest_for(&cache, &parent_head)?;
    let base_manifest = crate::commands::merge::find_lca_manifest_with_parent(
        &cache,
        &cfg.virtual_branch,
        &parent_name,
        Some(&parent_head),
    )?;

    let plan = plan_merge(
        &cache,
        cfg,
        token.as_deref(),
        &parent_name,
        &base_manifest,
        &branch_manifest,
        &parent_manifest,
    )
    .await?;

    if plan.merged.hash == branch_manifest.hash {
        output::info(&format!("Already up to date with '{parent_name}'."));
        return Ok(());
    }

    // Persist the merged manifest so `--continue` can rebuild the final tree
    // from it (clean trunk changes live here, not on disk).
    cache.store_manifest(&plan.merged)?;

    if plan.text_conflicts.is_empty() && plan.structural_conflicts.is_empty() {
        // Clean merge — land the sync commit immediately.
        let commit = finalize_sync(cfg, state_dir, &cache, &plan.merged, &parent_head, None)?;
        output::success(&format!(
            "Pulled '{parent_name}' into '{}' (commit {}).",
            cfg.virtual_branch,
            commit.short()
        ));
        return Ok(());
    }

    // Conflicts: materialize the marker files into the overlay (write through
    // the live mount so the FUSE layer captures them like ordinary edits), then
    // record the sync state and stop for the user to resolve.
    for (path, content, _mode) in &plan.text_conflicts {
        write_through_mount(cfg, path, content.as_bytes())?;
    }

    let sync = MountSyncState {
        parent_name: parent_name.clone(),
        parent_head: parent_head.0.clone(),
        merged_manifest: plan.merged.hash.0.clone(),
        conflicts: plan
            .text_conflicts
            .iter()
            .map(|(p, _, _)| p.clone())
            .collect(),
    };
    save_sync_state(state_dir, &sync)?;

    let total = plan.text_conflicts.len() + plan.structural_conflicts.len();
    output::warning(&format!(
        "Pull conflict: {total} file(s) need manual resolution"
    ));
    for (p, _, _) in &plan.text_conflicts {
        output::info(&format!("  CONFLICT (content): {p}"));
    }
    for p in &plan.structural_conflicts {
        output::info(&format!(
            "  CONFLICT (binary): {p} — kept the branch version"
        ));
    }
    output::info("");
    output::info("Edit the conflicting files to resolve them, then run `oak pull --continue`.");
    output::info("To discard the merge, run `oak pull --abort`.");

    Err(OakError::MergeConflict(total))
}

/// Finish a conflicted pull: verify the markers are gone, then land the sync
/// commit on top of the merged manifest.
fn continue_pull(cfg: &MountConfig, state_dir: &Path) -> Result<()> {
    let Some(sync) = load_sync_state(state_dir)? else {
        return Err(OakError::MergeFailed("No pull in progress.".into()));
    };
    let cache = SqliteRepository::open_relaxed(&cache_db_path(state_dir))?;

    let merged_hash = Hash(sync.merged_manifest.clone());
    let merged = cache.get_manifest(&merged_hash)?.ok_or_else(|| {
        OakError::Server(
            "merged manifest missing from cache — run `oak pull --abort` and retry.".into(),
        )
    })?;

    // Scan the recorded conflict files for leftover markers. Read the overlay
    // copy directly (the user's edits live there); a path that was never
    // touched still reads the marker blob and correctly fails the check.
    let overlay = load_overlay_meta(state_dir)?;
    let overlay_root = overlay_dir(state_dir);
    let mut still_conflicted = Vec::new();
    for path in &sync.conflicts {
        let content = read_current(
            cfg,
            state_dir,
            &overlay,
            &overlay_root,
            &cache,
            &merged,
            path,
        )?;
        if has_markers(&content) {
            still_conflicted.push(path.clone());
        }
    }
    if !still_conflicted.is_empty() {
        output::error(&format!(
            "{} file(s) still have conflict markers:",
            still_conflicted.len()
        ));
        for p in &still_conflicted {
            output::info(&format!("  {p}"));
        }
        output::info("Resolve them, then run `oak pull --continue`.");
        return Err(OakError::MergeConflict(still_conflicted.len()));
    }

    let parent_head = Hash(sync.parent_head.clone());
    let resolved_paths: HashSet<String> = sync.conflicts.iter().cloned().collect();
    let commit = finalize_sync(
        cfg,
        state_dir,
        &cache,
        &merged,
        &parent_head,
        Some(&resolved_paths),
    )?;
    clear_sync_state(state_dir)?;
    output::success(&format!(
        "Resolved and pulled '{}' into '{}' (commit {}).",
        sync.parent_name,
        cfg.virtual_branch,
        commit.short()
    ));
    Ok(())
}

/// Discard an in-progress pull: drop the overlay (reverting to the virtual
/// branch head) and the sync state.
fn abort_pull(cfg: &MountConfig, state_dir: &Path) -> Result<()> {
    if load_sync_state(state_dir)?.is_none() {
        return Err(OakError::MergeFailed("No pull in progress.".into()));
    }
    discard_overlay(state_dir)?;
    clear_sync_state(state_dir)?;
    let _ = cfg; // kept for symmetry / future use
    output::success("Pull aborted; conflict resolution discarded.");
    Ok(())
}

/// The full merged manifest plus the conflicts that need surfacing.
struct MergePlan {
    merged: Manifest,
    /// (path, marker content, mode) for text files that conflicted.
    text_conflicts: Vec<(String, String, FileMode)>,
    /// Binary / unreadable conflicts — the branch version is kept, no markers.
    structural_conflicts: Vec<String>,
}

fn same_manifest_entry(left: Option<&ManifestEntry>, right: Option<&ManifestEntry>) -> bool {
    match (left, right) {
        (Some(l), Some(r)) => l.blob_hash == r.blob_hash && l.mode == r.mode,
        (None, None) => true,
        _ => false,
    }
}

fn merge_text_mode(
    base: Option<&ManifestEntry>,
    branch: &ManifestEntry,
    parent: &ManifestEntry,
) -> Option<FileMode> {
    let base_mode = base.map(|e| e.mode);
    let branch_changed = Some(branch.mode) != base_mode;
    let parent_changed = Some(parent.mode) != base_mode;
    match (branch_changed, parent_changed) {
        (false, false) | (true, false) => Some(branch.mode),
        (false, true) => Some(parent.mode),
        (true, true) if branch.mode == parent.mode => Some(branch.mode),
        (true, true) => None,
    }
}

/// Run the 3-way manifest merge against the cache. Mirrors the non-mount
/// `sync_from_parent` merge loop, but returns a plan instead of writing a
/// working tree, and fetches conflicting blobs lazily.
async fn plan_merge(
    cache: &SqliteRepository,
    cfg: &MountConfig,
    token: Option<&str>,
    parent_name: &str,
    base: &Manifest,
    branch: &Manifest,
    parent: &Manifest,
) -> Result<MergePlan> {
    let base_map: HashMap<&str, &ManifestEntry> =
        base.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let branch_map: HashMap<&str, &ManifestEntry> = branch
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let parent_map: HashMap<&str, &ManifestEntry> = parent
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let all_paths: HashSet<&str> = base_map
        .keys()
        .chain(branch_map.keys())
        .chain(parent_map.keys())
        .copied()
        .collect();

    // First pass: figure out which blobs the text-conflict merge will need, and
    // fetch them in one batch so the second pass can read content locally.
    let mut needed: Vec<Hash> = Vec::new();
    for path in &all_paths {
        let base_entry = base_map.get(path).copied();
        let branch_entry = branch_map.get(path).copied();
        let parent_entry = parent_map.get(path).copied();
        let base_hash = base_entry.map(|e| &e.blob_hash);
        let branch_hash = branch_entry.map(|e| &e.blob_hash);
        let parent_hash = parent_entry.map(|e| &e.blob_hash);
        if !same_manifest_entry(branch_entry, base_entry)
            && !same_manifest_entry(parent_entry, base_entry)
            && branch_hash != parent_hash
            && branch_entry.is_some()
            && parent_entry.is_some()
        {
            if let Some(be) = branch_entry {
                needed.push(be.blob_hash.clone());
            }
            if let Some(pe) = parent_entry {
                needed.push(pe.blob_hash.clone());
            }
            if let Some(bh) = base_hash {
                needed.push(bh.clone());
            }
        }
    }
    if !needed.is_empty() {
        crate::commands::blob_fetch::ensure_blobs_local(
            cache,
            &cfg.remote_url,
            &cfg.owner,
            &cfg.repo,
            token,
            &needed,
        )
        .await?;
    }

    let mut merged_entries: Vec<ManifestEntry> = Vec::new();
    let mut text_conflicts: Vec<(String, String, FileMode)> = Vec::new();
    let mut structural_conflicts: Vec<String> = Vec::new();

    for path in &all_paths {
        let base_entry = base_map.get(path).copied();
        let branch_entry = branch_map.get(path).copied();
        let parent_entry = parent_map.get(path).copied();
        let base_hash = base_entry.map(|e| &e.blob_hash);
        let branch_hash = branch_entry.map(|e| &e.blob_hash);
        let parent_hash = parent_entry.map(|e| &e.blob_hash);

        let branch_changed = !same_manifest_entry(branch_entry, base_entry);
        let parent_changed = !same_manifest_entry(parent_entry, base_entry);

        match (branch_changed, parent_changed) {
            (false, false) => {
                if let Some(entry) = branch_entry.or(parent_entry) {
                    merged_entries.push(entry.clone());
                }
            }
            (true, false) => {
                if let Some(entry) = branch_entry {
                    merged_entries.push(entry.clone());
                }
            }
            (false, true) => {
                if let Some(entry) = parent_entry {
                    merged_entries.push(entry.clone());
                }
            }
            (true, true) => {
                if same_manifest_entry(branch_entry, parent_entry) {
                    if let Some(entry) = branch_entry.or(parent_entry) {
                        merged_entries.push(entry.clone());
                    }
                    continue;
                }
                if branch_hash == parent_hash {
                    if let Some(entry) = branch_entry {
                        merged_entries.push(entry.clone());
                    }
                    structural_conflicts.push(path.to_string());
                    continue;
                }
                match (branch_entry, parent_entry) {
                    (Some(be), Some(pe)) => {
                        let branch_blob = cache.get_blob(&be.blob_hash)?;
                        let parent_blob = cache.get_blob(&pe.blob_hash)?;
                        if let (Some(bb), Some(pb)) = (branch_blob, parent_blob) {
                            let base_text = base_hash
                                .and_then(|h| cache.get_blob(h).ok().flatten())
                                .and_then(|b| String::from_utf8(b.content).ok())
                                .unwrap_or_default();
                            let branch_text = String::from_utf8(bb.content.clone());
                            let parent_text = String::from_utf8(pb.content.clone());
                            match (branch_text, parent_text) {
                                (Ok(bt), Ok(pt)) => {
                                    let Some(merged_mode) = merge_text_mode(base_entry, be, pe)
                                    else {
                                        merged_entries.push(be.clone());
                                        structural_conflicts.push(path.to_string());
                                        continue;
                                    };
                                    let merged = crate::commands::merge::create_conflict_content(
                                        &base_text,
                                        &bt,
                                        &pt,
                                        &cfg.virtual_branch,
                                        parent_name,
                                    );
                                    // Non-overlapping edits to the same file
                                    // merge cleanly (no markers); only a genuine
                                    // line-level overlap is a conflict.
                                    let had_conflict =
                                        crate::commands::merge::content_has_conflict_markers(
                                            &merged,
                                        );
                                    let blob = Blob::new(merged.clone().into_bytes());
                                    cache.store_blob(&blob)?;
                                    merged_entries.push(ManifestEntry {
                                        path: path.to_string(),
                                        blob_hash: blob.hash,
                                        mode: merged_mode,
                                    });
                                    if had_conflict {
                                        text_conflicts.push((
                                            path.to_string(),
                                            merged,
                                            merged_mode,
                                        ));
                                    }
                                }
                                _ => {
                                    merged_entries.push(be.clone());
                                    structural_conflicts.push(path.to_string());
                                }
                            }
                        } else {
                            // Couldn't load both blobs — keep the branch version.
                            merged_entries.push(be.clone());
                            structural_conflicts.push(path.to_string());
                        }
                    }
                    // modify/delete: keep the surviving side, flag it.
                    (Some(be), None) => {
                        merged_entries.push(be.clone());
                        structural_conflicts.push(path.to_string());
                    }
                    (None, Some(pe)) => {
                        merged_entries.push(pe.clone());
                        structural_conflicts.push(path.to_string());
                    }
                    (None, None) => {}
                }
            }
        }
    }

    Ok(MergePlan {
        merged: Manifest::new(merged_entries),
        text_conflicts,
        structural_conflicts,
    })
}

/// Build the final tree (merged manifest + the user's overlay edits) and land a
/// single sync commit on the virtual branch, stamped with the trunk head as its
/// `merge_parent_hash`. Clears the overlay afterward.
fn finalize_sync(
    cfg: &MountConfig,
    state_dir: &Path,
    cache: &SqliteRepository,
    merged: &Manifest,
    parent_head: &Hash,
    overlay_paths: Option<&HashSet<String>>,
) -> Result<Hash> {
    let overlay = load_overlay_meta(state_dir)?;
    let overlay_root = overlay_dir(state_dir);
    let scoped = overlay_paths.map(|paths| {
        let filters: Vec<String> = paths.iter().cloned().collect();
        split_overlay(&overlay, &filters)
    });
    let commit_overlay = scoped
        .as_ref()
        .map(|(selected, _remaining)| selected.clone())
        .unwrap_or_else(|| overlay.clone());

    let mut dirty_blobs: HashMap<String, Hash> = HashMap::new();
    for (path, dirty) in &commit_overlay.dirty {
        let read_from = if dirty.in_place {
            cfg.mount_point.join(path)
        } else {
            overlay_root.join(&dirty.overlay_file)
        };
        let content = fs::read(&read_from).map_err(OakError::Io)?;
        let blob = Blob::new(content);
        cache.store_blob(&blob)?;
        dirty_blobs.insert(path.clone(), blob.hash);
    }

    let new_manifest = build_committed_manifest(merged, &commit_overlay, &dirty_blobs);

    let branch_head = cache
        .get_branch_head(&cfg.virtual_branch)?
        .ok_or_else(|| OakError::Server("virtual branch has no head".into()))?;
    let branch_manifest = manifest_for(cache, &branch_head)?;
    let changes = branch_manifest.diff(&new_manifest);

    let files: Vec<FileChange> = changes
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

    let commit_hash = cache.put_commit_and_advance_refs(
        cfg.virtual_branch.clone(),
        Some(branch_head),
        Some(parent_head.clone()),
        new_manifest.entries.clone(),
        crate::commands::commit::get_author(),
        None,
        Utc::now(),
        files,
    )?;

    if let Some((_selected, remaining)) = scoped {
        for dirty in commit_overlay.dirty.values() {
            if !dirty.in_place && !dirty.overlay_file.is_empty() {
                let _ = fs::remove_file(overlay_root.join(&dirty.overlay_file));
            }
        }
        save_overlay_meta(state_dir, &remaining)?;
    } else {
        // Clear the overlay — its contents are now folded into the sync commit.
        // Rewriting overlay-meta.json also bumps its signature, so the live FUSE
        // layer reconciles to the new branch head on its next op.
        discard_overlay(state_dir)?;
    }

    Ok(commit_hash)
}

/// Wipe the overlay (metadata + flat content files) back to empty.
fn discard_overlay(state_dir: &Path) -> Result<()> {
    save_overlay_meta(state_dir, &OverlayMeta::default())?;
    let overlay_root = overlay_dir(state_dir);
    if let Ok(rd) = fs::read_dir(&overlay_root) {
        for ent in rd.flatten() {
            let _ = fs::remove_file(ent.path());
        }
    }
    Ok(())
}

/// Read a conflict path's *current* content: the overlay copy if the user has
/// it dirty, otherwise the merged-manifest blob (still markered — which is the
/// point: an untouched conflict must fail the resolution check).
fn read_current(
    cfg: &MountConfig,
    _state_dir: &Path,
    overlay: &OverlayMeta,
    overlay_root: &Path,
    cache: &SqliteRepository,
    merged: &Manifest,
    path: &str,
) -> Result<String> {
    if overlay.deletions.iter().any(|deleted| deleted == path) {
        return Ok(String::new());
    }
    if let Some(dirty) = overlay.dirty.get(path) {
        let read_from = if dirty.in_place {
            cfg.mount_point.join(path)
        } else {
            overlay_root.join(&dirty.overlay_file)
        };
        let bytes = fs::read(&read_from).map_err(OakError::Io)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    if let Some(entry) = merged.entries.iter().find(|e| e.path == path) {
        if let Some(blob) = cache.get_blob(&entry.blob_hash)? {
            return Ok(String::from_utf8_lossy(&blob.content).into_owned());
        }
    }
    Ok(String::new())
}

/// Write `content` to `path` through the live mount point so the FUSE layer
/// captures it into the overlay exactly like a user edit.
fn write_through_mount(cfg: &MountConfig, path: &str, content: &[u8]) -> Result<()> {
    let full = cfg.mount_point.join(path);
    if let Some(parent) = full.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&full, content).map_err(OakError::Io)
}

/// Load the manifest for a commit hash from the cache.
fn manifest_for(cache: &SqliteRepository, commit_hash: &Hash) -> Result<Manifest> {
    let commit = cache
        .get_commit(commit_hash)?
        .ok_or_else(|| OakError::CommitNotFound(commit_hash.to_string()))?;
    cache
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))
}

/// Fetch the trunk's mainline into the cache, from `head` back to the first
/// commit already present (the mount's fork point or a prior sync's merge
/// parent). Only newly-fetched commits are expanded, so the walk stops at the
/// existing cache boundary instead of pulling the trunk's entire history.
///
/// Follows `parent_hash` only — *not* `merge_parent_hash`. A squash commit on
/// the trunk records the merged feature branch's original tip as its merge
/// parent, and those tips aren't always retrievable via `/commits/info` (the
/// branch may be closed/pruned), so chasing them would abort the whole fetch.
/// The merge base always lies on the trunk's `parent_hash` mainline, which is
/// all the LCA walk needs. If a mainline ancestor can't be fetched we stop
/// gracefully: the merge base may then be missed (over-reporting conflicts) but
/// the pull still runs instead of hard-failing.
async fn fetch_parent_history(
    cache: &SqliteRepository,
    cfg: &MountConfig,
    token: Option<&str>,
    head: &Hash,
) -> Result<()> {
    let mut frontier = vec![head.clone()];
    // Safety bound: a mount that has fallen thousands of commits behind the
    // trunk is pathological; stop rather than loop unbounded.
    for _ in 0..10_000 {
        let mut missing: Vec<Hash> = Vec::new();
        for h in &frontier {
            if cache.get_commit(h)?.is_none() {
                missing.push(h.clone());
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        if let Err(e) = crate::commands::blob_fetch::ensure_commits_local(
            cache,
            &cfg.remote_url,
            &cfg.owner,
            &cfg.repo,
            token,
            &missing,
        )
        .await
        {
            // Couldn't fetch this layer — stop walking and merge with whatever
            // history we already have rather than failing the pull outright.
            output::info(&format!("  (stopped fetching trunk history early: {e})"));
            return Ok(());
        }

        let mut next: Vec<Hash> = Vec::new();
        for h in &missing {
            if let Some(commit) = cache.get_commit(h)? {
                if let Some(p) = commit.parent_hash {
                    next.push(p);
                }
            }
        }
        next.sort_by(|a, b| a.0.cmp(&b.0));
        next.dedup_by(|a, b| a.0 == b.0);
        frontier = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_cfg() -> MountConfig {
        MountConfig {
            id: "t".into(),
            mount_point: std::path::PathBuf::from("/tmp/oak-pull-test-none"),
            remote_url: "http://invalid.invalid".into(),
            owner: "o".into(),
            repo: "r".into(),
            base_branch: "main".into(),
            base_commit: "x".into(),
            virtual_branch: "feat--abcd1234".into(),
        }
    }

    fn open_cache(dir: &TempDir) -> SqliteRepository {
        SqliteRepository::open_relaxed(&dir.path().join("cache.db")).unwrap()
    }

    fn store(repo: &SqliteRepository, content: &str) -> Hash {
        let blob = Blob::new(content.as_bytes().to_vec());
        repo.store_blob(&blob).unwrap();
        blob.hash
    }

    fn entry(path: &str, hash: Hash) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            blob_hash: hash,
            mode: FileMode::Regular,
        }
    }

    fn entry_with_mode(path: &str, hash: Hash, mode: FileMode) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            blob_hash: hash,
            mode,
        }
    }

    #[test]
    fn marker_detection() {
        assert!(!has_markers("clean line\nanother\n"));
        let conflicted = format!(
            "{}\nmine\n=======\ntheirs\n{}\n",
            "<".repeat(7),
            ">".repeat(7)
        );
        assert!(has_markers(&conflicted));
    }

    #[test]
    fn marker_detection_ignores_embedded_marker_text() {
        let embedded = format!(
            "docs mention {} and {} as strings\n",
            "<".repeat(7),
            ">".repeat(7)
        );
        assert!(!has_markers(&embedded));

        let only_separator = "generated text\n=======\nnot a conflict\n";
        assert!(!has_markers(only_separator));
    }

    #[tokio::test]
    async fn clean_parent_change_folds_in() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "a\n");
        let parent_h = store(&repo, "a\nb\n");
        let base = Manifest::new(vec![entry("f.txt", base_h.clone())]);
        let branch = Manifest::new(vec![entry("f.txt", base_h.clone())]); // unchanged on branch
        let parent = Manifest::new(vec![entry("f.txt", parent_h.clone())]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert!(plan.structural_conflicts.is_empty());
        assert_eq!(plan.merged.entries.len(), 1);
        assert_eq!(plan.merged.entries[0].blob_hash, parent_h);
        assert_ne!(
            plan.merged.hash, branch.hash,
            "merge should advance the tree"
        );
    }

    #[tokio::test]
    async fn parent_mode_only_change_folds_in() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "#!/bin/sh\n");
        let base = Manifest::new(vec![entry_with_mode(
            "run.sh",
            base_h.clone(),
            FileMode::Regular,
        )]);
        let branch = Manifest::new(vec![entry_with_mode(
            "run.sh",
            base_h.clone(),
            FileMode::Regular,
        )]);
        let parent = Manifest::new(vec![entry_with_mode(
            "run.sh",
            base_h,
            FileMode::Executable,
        )]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert!(plan.structural_conflicts.is_empty());
        assert_eq!(plan.merged.entries.len(), 1);
        assert_eq!(plan.merged.entries[0].mode, FileMode::Executable);
        assert_ne!(
            plan.merged.hash, branch.hash,
            "mode-only parent change should advance the tree"
        );
    }

    #[tokio::test]
    async fn divergent_mode_only_changes_conflict() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "target\n");
        let base = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            base_h.clone(),
            FileMode::Regular,
        )]);
        let branch = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            base_h.clone(),
            FileMode::Executable,
        )]);
        let parent = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            base_h,
            FileMode::Symlink,
        )]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert_eq!(plan.structural_conflicts, vec!["link-or-script"]);
        assert_eq!(plan.merged.entries.len(), 1);
        assert_eq!(plan.merged.entries[0].mode, FileMode::Executable);
    }

    #[tokio::test]
    async fn branch_content_and_parent_mode_merge_preserves_parent_mode() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "#!/bin/sh\necho base\n");
        let branch_h = store(&repo, "#!/bin/sh\necho branch\n");
        let base = Manifest::new(vec![entry_with_mode(
            "run.sh",
            base_h.clone(),
            FileMode::Regular,
        )]);
        let branch = Manifest::new(vec![entry_with_mode("run.sh", branch_h, FileMode::Regular)]);
        let parent = Manifest::new(vec![entry_with_mode(
            "run.sh",
            base_h,
            FileMode::Executable,
        )]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert!(plan.structural_conflicts.is_empty());
        assert_eq!(plan.merged.entries.len(), 1);
        assert_eq!(
            plan.merged.entries[0].mode,
            FileMode::Executable,
            "parent chmod must survive a clean text merge with branch content"
        );
        let blob = repo
            .get_blob(&plan.merged.entries[0].blob_hash)
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(blob.content).unwrap(),
            "#!/bin/sh\necho branch\n"
        );
    }

    #[tokio::test]
    async fn both_sides_change_mode_differently_stays_structural_conflict() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "target\n");
        let branch_h = store(&repo, "branch target\n");
        let base = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            base_h.clone(),
            FileMode::Regular,
        )]);
        let branch = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            branch_h,
            FileMode::Executable,
        )]);
        let parent = Manifest::new(vec![entry_with_mode(
            "link-or-script",
            base_h,
            FileMode::Symlink,
        )]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert_eq!(plan.structural_conflicts, vec!["link-or-script"]);
    }

    #[tokio::test]
    async fn overlapping_change_conflicts() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "base\n");
        let branch_h = store(&repo, "branch side\n");
        let parent_h = store(&repo, "parent side\n");
        let base = Manifest::new(vec![entry("f.txt", base_h)]);
        let branch = Manifest::new(vec![entry("f.txt", branch_h)]);
        let parent = Manifest::new(vec![entry("f.txt", parent_h)]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert_eq!(plan.text_conflicts.len(), 1);
        let (path, content, _mode) = &plan.text_conflicts[0];
        assert_eq!(path, "f.txt");
        assert!(
            has_markers(content),
            "conflict content should carry markers"
        );
        assert!(content.contains("branch side") && content.contains("parent side"));
        // The merged manifest points at the stored marker blob.
        assert_eq!(plan.merged.entries.len(), 1);
    }

    #[tokio::test]
    async fn already_up_to_date_when_branch_has_change() {
        let tmp = TempDir::new().unwrap();
        let repo = open_cache(&tmp);
        let base_h = store(&repo, "a\n");
        let parent_h = store(&repo, "a\nb\n");
        let base = Manifest::new(vec![entry("f.txt", base_h)]);
        // Branch already contains exactly the parent's change.
        let branch = Manifest::new(vec![entry("f.txt", parent_h.clone())]);
        let parent = Manifest::new(vec![entry("f.txt", parent_h)]);

        let plan = plan_merge(&repo, &test_cfg(), None, "main", &base, &branch, &parent)
            .await
            .unwrap();

        assert!(plan.text_conflicts.is_empty());
        assert_eq!(
            plan.merged.hash, branch.hash,
            "no divergence → merged tree equals the branch tree"
        );
    }

    /// Build a state dir with a cache holding one virtual-branch commit
    /// (`shared.txt` = "base\n"). Returns the cache, a matching config, and the
    /// base commit hash (reused as a stand-in trunk head / merge parent).
    fn setup_state(dir: &TempDir) -> (SqliteRepository, MountConfig, Hash) {
        let state_dir = dir.path();
        let cache = SqliteRepository::open_relaxed(&cache_db_path(state_dir)).unwrap();
        let vb = "feat--abcd1234".to_string();
        cache
            .store_branch(&oak_core::Branch::new(
                vb.clone(),
                Some("feat".into()),
                Some("main".into()),
            ))
            .unwrap();
        let base_h = store(&cache, "base\n");
        let manifest = Manifest::new(vec![entry("shared.txt", base_h)]);
        let mh = cache.put_manifest(manifest.entries.clone()).unwrap();
        let commit_h = cache
            .put_commit(
                vb.clone(),
                None,
                None,
                mh,
                "tester".into(),
                None,
                Utc::now(),
                vec![],
            )
            .unwrap();
        cache.set_branch_head(&vb, &commit_h).unwrap();
        cache.set_head(&commit_h).unwrap();

        let mut cfg = test_cfg();
        cfg.virtual_branch = vb;
        cfg.mount_point = state_dir.to_path_buf();
        (cache, cfg, commit_h)
    }

    /// Stage a merged manifest in the cache + an overlay copy of `shared.txt`,
    /// plus the sync state — i.e. the exact on-disk shape a conflicted pull
    /// leaves behind. `overlay_content` is what the user is "looking at".
    fn stage_conflict(
        cache: &SqliteRepository,
        state_dir: &Path,
        parent_head: &Hash,
        overlay_content: &str,
    ) -> Hash {
        // Merged manifest still points at a marker blob for the conflict path.
        let marker = format!(
            "{}\nmine\n=======\ntheirs\n{}\n",
            "<".repeat(7),
            ">".repeat(7)
        );
        let marker_h = store(cache, &marker);
        let merged = Manifest::new(vec![entry("shared.txt", marker_h)]);
        cache.store_manifest(&merged).unwrap();

        let overlay_root = overlay_dir(state_dir);
        fs::create_dir_all(&overlay_root).unwrap();
        fs::write(overlay_root.join("shared.txt"), overlay_content).unwrap();
        let mut meta = OverlayMeta::default();
        meta.dirty.insert(
            "shared.txt".into(),
            super::super::state::DirtyEntry {
                overlay_file: "shared.txt".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        save_overlay_meta(state_dir, &meta).unwrap();

        save_sync_state(
            state_dir,
            &MountSyncState {
                parent_name: "main".into(),
                parent_head: parent_head.0.clone(),
                merged_manifest: merged.hash.0.clone(),
                conflicts: vec!["shared.txt".into()],
            },
        )
        .unwrap();
        merged.hash
    }

    #[test]
    fn abort_clears_overlay_and_sync_state() {
        let dir = TempDir::new().unwrap();
        let (cache, cfg, base) = setup_state(&dir);
        stage_conflict(&cache, dir.path(), &base, "anything\n");

        abort_pull(&cfg, dir.path()).unwrap();

        assert!(load_sync_state(dir.path()).unwrap().is_none());
        let ov = load_overlay_meta(dir.path()).unwrap();
        assert!(ov.dirty.is_empty(), "overlay should be reverted");
        // Branch head unmoved (no commit on abort).
        assert_eq!(
            cache.get_branch_head(&cfg.virtual_branch).unwrap(),
            Some(base)
        );
    }

    #[test]
    fn continue_rejects_remaining_markers() {
        let dir = TempDir::new().unwrap();
        let (cache, cfg, base) = setup_state(&dir);
        let markered = format!(
            "{}\nmine\n=======\ntheirs\n{}\n",
            "<".repeat(7),
            ">".repeat(7)
        );
        stage_conflict(&cache, dir.path(), &base, &markered);

        let err = continue_pull(&cfg, dir.path()).unwrap_err();
        assert!(matches!(err, OakError::MergeConflict(_)), "got {err:?}");
        // State is preserved so the user can keep editing.
        assert!(load_sync_state(dir.path()).unwrap().is_some());
        assert_eq!(
            cache.get_branch_head(&cfg.virtual_branch).unwrap(),
            Some(base)
        );
    }

    #[test]
    fn continue_finalizes_resolved_merge() {
        let dir = TempDir::new().unwrap();
        let (cache, cfg, base) = setup_state(&dir);
        stage_conflict(&cache, dir.path(), &base, "resolved!\n");

        continue_pull(&cfg, dir.path()).unwrap();

        // Sync state + overlay are cleared.
        assert!(load_sync_state(dir.path()).unwrap().is_none());
        assert!(load_overlay_meta(dir.path()).unwrap().dirty.is_empty());

        // A new commit landed, parented on the old head, stamped with the trunk
        // head as its merge parent, carrying the resolved content.
        let head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
        assert_ne!(head, base, "a sync commit should have landed");
        let commit = cache.get_commit(&head).unwrap().unwrap();
        assert_eq!(commit.parent_hash, Some(base.clone()));
        assert_eq!(commit.merge_parent_hash, Some(base));
        let manifest = cache.get_manifest(&commit.manifest_hash).unwrap().unwrap();
        let e = manifest
            .entries
            .iter()
            .find(|e| e.path == "shared.txt")
            .unwrap();
        let blob = cache.get_blob(&e.blob_hash).unwrap().unwrap();
        assert_eq!(String::from_utf8(blob.content).unwrap(), "resolved!\n");
        assert!(!has_markers("resolved!\n"));
    }

    #[test]
    fn continue_accepts_deleted_conflict_resolution() {
        let dir = TempDir::new().unwrap();
        let (cache, cfg, base) = setup_state(&dir);
        stage_conflict(&cache, dir.path(), &base, "resolved!\n");

        let overlay_root = overlay_dir(dir.path());
        let mut meta = load_overlay_meta(dir.path()).unwrap();
        let dirty = meta.dirty.remove("shared.txt").unwrap();
        fs::remove_file(overlay_root.join(dirty.overlay_file)).unwrap();
        meta.deletions.push("shared.txt".into());
        save_overlay_meta(dir.path(), &meta).unwrap();

        continue_pull(&cfg, dir.path()).unwrap();

        let head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
        let commit = cache.get_commit(&head).unwrap().unwrap();
        let manifest = cache.get_manifest(&commit.manifest_hash).unwrap().unwrap();
        assert!(
            !manifest.entries.iter().any(|e| e.path == "shared.txt"),
            "deleting a conflict path should commit the deletion resolution"
        );
        let remaining = load_overlay_meta(dir.path()).unwrap();
        assert!(
            !remaining.deletions.contains(&"shared.txt".to_string()),
            "the consumed conflict deletion should not stay dirty"
        );
        assert!(load_sync_state(dir.path()).unwrap().is_none());
    }

    #[test]
    fn continue_finalizes_only_recorded_conflict_overlay_paths() {
        let dir = TempDir::new().unwrap();
        let (cache, cfg, base) = setup_state(&dir);
        stage_conflict(&cache, dir.path(), &base, "resolved!\n");

        let overlay_root = overlay_dir(dir.path());
        fs::write(overlay_root.join("unrelated.txt"), "draft\n").unwrap();
        let mut meta = load_overlay_meta(dir.path()).unwrap();
        meta.dirty.insert(
            "unrelated.txt".into(),
            super::super::state::DirtyEntry {
                overlay_file: "unrelated.txt".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        save_overlay_meta(dir.path(), &meta).unwrap();

        continue_pull(&cfg, dir.path()).unwrap();

        let head = cache.get_branch_head(&cfg.virtual_branch).unwrap().unwrap();
        let commit = cache.get_commit(&head).unwrap().unwrap();
        let manifest = cache.get_manifest(&commit.manifest_hash).unwrap().unwrap();
        assert!(
            manifest.entries.iter().any(|e| e.path == "shared.txt"),
            "resolved conflict should be committed"
        );
        assert!(
            !manifest.entries.iter().any(|e| e.path == "unrelated.txt"),
            "unrelated overlay edits made during conflict resolution must not enter sync commit"
        );
        let remaining = load_overlay_meta(dir.path()).unwrap();
        assert!(
            !remaining.dirty.contains_key("shared.txt"),
            "resolved conflict overlay should be consumed by the sync commit"
        );
        assert!(
            remaining.dirty.contains_key("unrelated.txt"),
            "unrelated overlay edits should remain dirty after the sync commit"
        );
        assert!(
            overlay_root.join("unrelated.txt").exists(),
            "unrelated overlay content should remain available for a later commit"
        );
    }
}
