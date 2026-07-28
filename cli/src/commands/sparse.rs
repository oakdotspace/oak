//! `oak sparse` — manage a Perforce-style sparse-checkout cone.
//!
//! A sparse checkout scopes the working tree to a set of path prefixes (the
//! *cone*, persisted in [`MetadataKey::SparsePaths`]). Files outside the cone
//! are not written to disk and their content is not downloaded, but they remain
//! in the repository and are carried forward verbatim by commits — narrowing a
//! checkout never deletes the paths it drops from the server's history.
//!
//! Subcommands re-sync the working tree to the new cone in place: paths that
//! leave the cone are removed from disk, paths that enter it are materialized
//! from whatever blobs are already local. Newly-included files whose content
//! was never downloaded (a fresh sparse clone never fetched them) are reported
//! so the user can run `oak pull` to hydrate them.

use std::path::Path;

use oak_core::{MetadataKey, OakError, Repository, Result, SparseCone, SqliteRepository};

use crate::materialize::{apply_manifest, ApplyOpts, MissingBlobs};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// What `oak sparse` should do. Mirrors the clap subcommands in `main.rs`.
pub enum SparseAction {
    /// Print the active cone (or "full checkout").
    List { json: bool },
    /// Replace the cone with exactly these prefixes.
    Set { paths: Vec<String> },
    /// Union these prefixes into the existing cone.
    Add { paths: Vec<String> },
    /// Drop the cone entirely and return to a full checkout.
    Disable,
}

pub fn run(cwd: &Path, action: SparseAction) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let root = ctx.work_tree.clone();
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    let current =
        SparseCone::from_metadata(repo.get_metadata(MetadataKey::SparsePaths)?.as_deref());

    match action {
        SparseAction::List { json } => list(&current, json),
        SparseAction::Set { paths } => {
            let next = SparseCone::new(paths.iter().flat_map(|p| p.split(',')));
            if next.is_none() {
                return Err(OakError::InvalidArgument(
                    "`oak sparse set` needs at least one path prefix (use `oak sparse disable` to clear)"
                        .to_string(),
                ));
            }
            apply_cone(&ctx.oak_dir, &root, &repo, &current, next)
        }
        SparseAction::Add { paths } => {
            let additions = paths.iter().flat_map(|p| p.split(','));
            let next = match &current {
                Some(c) => Some(c.with_added(additions)),
                None => SparseCone::new(additions),
            };
            if next.is_none() {
                return Err(OakError::InvalidArgument(
                    "`oak sparse add` needs at least one path prefix".to_string(),
                ));
            }
            apply_cone(&ctx.oak_dir, &root, &repo, &current, next)
        }
        SparseAction::Disable => {
            if current.is_none() {
                output::info("Already a full checkout — nothing to disable.");
                return Ok(());
            }
            apply_cone(&ctx.oak_dir, &root, &repo, &current, None)
        }
    }
}

fn list(current: &Option<SparseCone>, json: bool) -> Result<()> {
    match current {
        Some(cone) => {
            if json {
                let arr = serde_json::to_string(cone.prefixes())?;
                println!("{{\"sparse\":true,\"paths\":{arr}}}");
            } else {
                output::info("Sparse checkout — scoped to:");
                for p in cone.prefixes() {
                    println!("  {p}");
                }
            }
        }
        None => {
            if json {
                println!("{{\"sparse\":false,\"paths\":[]}}");
            } else {
                output::info("Full checkout (no sparse cone set).");
            }
        }
    }
    Ok(())
}

/// Persist `next` and re-sync the working tree from HEAD to match it: delete
/// tracked files that leave the cone, then materialize what's now in it.
fn apply_cone(
    oak_dir: &Path,
    root: &Path,
    repo: &SqliteRepository,
    current: &Option<SparseCone>,
    next: Option<SparseCone>,
) -> Result<()> {
    let lock = WorkdirLock::acquire(oak_dir)?;

    // Refuse to silently discard in-flight work: a re-sync rewrites the working
    // tree, so require a clean tree first (within the *current* cone — that's
    // all `oak status` reports anyway).
    if !crate::commands::commit::worktree_is_clean_without_storing_blobs(repo, root)? {
        return Err(OakError::DirtyWorkingTree(
            "working tree has uncommitted changes; commit or reset before changing the sparse cone"
                .to_string(),
        ));
    }

    let head = match repo.get_current_branch_name().ok().flatten() {
        Some(name) => repo.get_branch_head(&name)?,
        None => repo.get_head()?,
    };

    // No commits yet: just record the cone; there's nothing to materialize.
    let Some(head) = head else {
        write_cone_metadata(repo, &next)?;
        report_change(current, &next);
        return Ok(());
    };

    let commit = repo
        .get_commit(&head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;

    // Remove tracked files that are leaving the cone. `apply_manifest` won't do
    // this — once the new (narrower) cone is stored it treats those paths as
    // out of scope and never touches them — so delete them here, before storing
    // the new cone. Only manifest (tracked) paths are removed; untracked user
    // files are left alone.
    let mut removed: Vec<String> = Vec::new();
    for entry in &manifest.entries {
        let in_next = next.as_ref().is_none_or(|c| c.covers(&entry.path));
        if in_next {
            continue;
        }
        let file_path = root.join(&entry.path);
        match std::fs::symlink_metadata(&file_path) {
            Ok(md) if !md.is_dir() => std::fs::remove_file(&file_path)?,
            _ => {}
        }
        removed.push(entry.path.clone());
    }
    if !removed.is_empty() {
        repo.update_stat_cache(&[], &removed)?;
    }

    // Store the new cone so `apply_manifest` scopes materialization to it.
    write_cone_metadata(repo, &next)?;

    // Materialize what's now in the cone. Newly-included files whose blobs were
    // never downloaded (a sparse clone skipped them) are skipped with a notice
    // rather than failing — `oak pull` hydrates them.
    let report = apply_manifest(
        &lock,
        root,
        repo,
        &manifest,
        ApplyOpts {
            missing_blobs: MissingBlobs::Skip,
            ..ApplyOpts::default()
        },
    )?;

    report_change(current, &next);
    if !removed.is_empty() {
        output::info(&format!(
            "Removed {} file(s) now outside the cone.",
            removed.len()
        ));
    }
    if !report.skipped.is_empty() {
        output::warning(&format!(
            "{} newly-included file(s) aren't downloaded yet — run `oak pull` to hydrate them:",
            report.skipped.len()
        ));
        for p in &report.skipped {
            output::warning(&format!("  - {p}"));
        }
    }
    Ok(())
}

/// Persist the cone (empty value == full checkout, since there's no
/// metadata-delete API; `SparseCone::from_metadata` reads it back as `None`).
fn write_cone_metadata(repo: &SqliteRepository, cone: &Option<SparseCone>) -> Result<()> {
    let value = cone.as_ref().map(|c| c.to_metadata()).unwrap_or_default();
    repo.set_metadata(MetadataKey::SparsePaths, &value)
}

fn report_change(current: &Option<SparseCone>, next: &Option<SparseCone>) {
    match next {
        Some(cone) => {
            output::success(&format!(
                "Sparse cone set to {} prefix(es): {}",
                cone.prefixes().len(),
                cone.prefixes().join(", ")
            ));
        }
        None => {
            let _ = current;
            output::success("Sparse checkout disabled — back to a full checkout.");
        }
    }
}
