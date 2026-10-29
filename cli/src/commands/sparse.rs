//! `oak sparse` — manage a Perforce-style sparse-checkout cone.
//!
//! A sparse checkout scopes the working tree to a set of path prefixes (the
//! *cone*, persisted in [`MetadataKey::SparsePaths`]). Files outside the cone
//! are not written to disk and their content is not downloaded, but they remain
//! in the repository and are carried forward verbatim by commits — narrowing a
//! checkout never deletes the paths it drops from the server's history.
//!
//! Subcommands re-sync the working tree to the new cone in place: paths that
//! leave the cone are removed from disk, and paths that enter it are
//! downloaded (when a sparse clone never fetched them) and written. A change
//! refuses, leaving everything as it was, when uncommitted or untracked work
//! sits in the old or new cone, or when entering content can't be downloaded.
//! Content the server withholds under path permissions is skipped with an
//! access note, never treated as a failure.

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
                output::print_line(&format!("{{\"sparse\":true,\"paths\":{arr}}}"));
            } else {
                output::info("Sparse checkout — scoped to:");
                for p in cone.prefixes() {
                    output::print_line(&format!("  {p}"));
                }
            }
        }
        None => {
            if json {
                output::print_line("{\"sparse\":false,\"paths\":[]}");
            } else {
                output::info("Full checkout (no sparse cone set).");
            }
        }
    }
    Ok(())
}

/// Persist `next` and re-sync the working tree from HEAD to match it: delete
/// tracked files that leave the cone, hydrate and write what enters it.
///
/// Refuses (changing nothing) when the old or new cone holds uncommitted or
/// untracked work — a re-sync rewrites and deletes paths on both sides, so a
/// guard scoped to the old cone alone would silently overwrite edits under a
/// newly added prefix — and when content entering the cone can't be
/// downloaded (other than content the server withholds under path
/// permissions, which is skipped with an access note).
fn apply_cone(
    oak_dir: &Path,
    root: &Path,
    repo: &SqliteRepository,
    current: &Option<SparseCone>,
    next: Option<SparseCone>,
) -> Result<()> {
    let lock = WorkdirLock::acquire(oak_dir)?;

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

    let in_current = |path: &str| current.as_ref().is_none_or(|c| c.covers(path));
    let in_next = |path: &str| next.as_ref().is_none_or(|c| c.covers(path));
    refuse_dirty_union(repo, root, &manifest, &in_current, &in_next)?;

    // Download what enters the cone before touching anything, so a failure
    // leaves the old cone and working tree exactly as they were. Previously
    // withheld hashes are asked about again: a fresh grant lands here.
    let entering = |path: &str| in_next(path) && !in_current(path);
    let to_fetch = super::hydrate::unhydrated_blobs(repo, &manifest, &|path| in_next(path))?;
    if !to_fetch.is_empty() {
        hydrate_or_refuse(repo, &manifest, &to_fetch, &entering)?;
    }

    // Remove tracked files that are leaving the cone. `apply_manifest` won't do
    // this — once the new (narrower) cone is stored it treats those paths as
    // out of scope and never touches them — so delete them here, before storing
    // the new cone. Only manifest (tracked) paths are removed; the dirty guard
    // above proved each one still holds HEAD's content.
    let mut removed: Vec<String> = Vec::new();
    for entry in &manifest.entries {
        if in_next(&entry.path) {
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
        let ignore = oak_core::IgnorePatterns::new(root)?;
        crate::materialize::prune_emptied_dirs(root, &ignore, &removed)?;
    }

    // Store the new cone so `apply_manifest` scopes materialization to it.
    write_cone_metadata(repo, &next)?;

    // Materialize what's now in the cone. Only withheld content (restricted
    // or declared lost) can still be missing here; it is skipped with a note.
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
        let restricted: std::collections::HashSet<String> =
            super::restricted::restricted_paths_in_manifest(repo, &manifest)
                .into_iter()
                .collect();
        let (withheld, other): (Vec<&String>, Vec<&String>) = report
            .skipped
            .iter()
            .partition(|p| restricted.contains(p.as_str()));
        if !withheld.is_empty() {
            output::warning(&super::restricted::withheld_summary(withheld.len()));
            for p in &withheld {
                output::warning(&format!("  - {p}"));
            }
        }
        if !other.is_empty() {
            output::warning(&format!(
                "{} file(s) in the cone have no local content (declared lost, or never downloaded — run `oak pull`):",
                other.len()
            ));
            for p in &other {
                output::warning(&format!("  - {p}"));
            }
        }
    }
    Ok(())
}

/// Refuse a cone change while uncommitted or untracked work sits anywhere in
/// the old or new cone. A tracked file missing from disk counts only where
/// the old cone should have written it.
fn refuse_dirty_union(
    repo: &SqliteRepository,
    root: &Path,
    manifest: &oak_core::Manifest,
    in_current: &dyn Fn(&str) -> bool,
    in_next: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let withheld: std::collections::HashSet<String> =
        super::restricted::restricted_paths_in_manifest(repo, manifest)
            .into_iter()
            .chain(super::known_loss::known_lost_paths_in_manifest(
                repo, manifest,
            ))
            .collect();
    let expected_on_disk = |entry: &oak_core::ManifestEntry| {
        in_current(&entry.path)
            && !withheld.contains(&entry.path)
            && repo.has_blob(&entry.blob_hash).unwrap_or(false)
    };
    let dirty = crate::commands::commit::dirty_paths_in_scope(
        repo,
        root,
        manifest,
        &|path| in_current(path) || in_next(path),
        &expected_on_disk,
    )?;
    if dirty.is_empty() {
        return Ok(());
    }
    const SHOWN: usize = 10;
    let mut list = dirty
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if dirty.len() > SHOWN {
        list.push_str(&format!(" (+{} more)", dirty.len() - SHOWN));
    }
    Err(OakError::DirtyWorkingTree(format!(
        "{} uncommitted or untracked file(s) inside the old or new sparse cone would be overwritten or deleted by the re-sync: {list}. Commit them (widen the cone first if they're outside it), reset them, or move them out of the repo before changing the cone",
        dirty.len()
    )))
}

/// Fetch `to_fetch` from the linked remote. Content the server withholds is
/// recorded and skipped; anything else that stays missing for a path
/// entering the cone refuses the change.
fn hydrate_or_refuse(
    repo: &SqliteRepository,
    manifest: &oak_core::Manifest,
    to_fetch: &[oak_core::Hash],
    entering: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let fetch_error = match super::hydrate::FetchSource::linked(repo)? {
        Some(source) => {
            output::info(&format!(
                "Downloading {} file(s) for the new cone from {}…",
                to_fetch.len(),
                source.remote
            ));
            let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
            rt.block_on(super::hydrate::fetch_blobs(repo, &source, to_fetch))
                .err()
        }
        None => None,
    };
    let restricted = super::restricted::load_restricted_blobs(repo);
    let wanted: std::collections::HashSet<&str> = to_fetch.iter().map(|h| h.as_str()).collect();
    let mut missing: Vec<&str> = manifest
        .entries
        .iter()
        .filter(|e| {
            entering(&e.path)
                && wanted.contains(e.blob_hash.as_str())
                && !restricted.contains(e.blob_hash.as_str())
                && !repo.has_blob(&e.blob_hash).unwrap_or(false)
        })
        .map(|e| e.path.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_unstable();
    let first = missing[0];
    let cause = match fetch_error {
        Some(error) => format!("download failed: {error}"),
        None => "no remote is configured to download them from".to_string(),
    };
    Err(OakError::Server(format!(
        "{} file(s) entering the sparse cone have no local content ({cause}); the cone was not changed. First: {first}",
        missing.len()
    )))
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
