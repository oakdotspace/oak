//! Hydrating working-tree files that were never written: the in-cone files a
//! sparse clone (or a sparse cone change) never downloaded, and files whose
//! content the server withheld under path-based permissions until an admin
//! granted access.
//!
//! Both shapes are a tracked path that is absent on disk *by design* — not a
//! user deletion. This module finds them, asks the server for their content
//! (tolerating content that is still withheld), and writes exactly the files
//! that are still absent, so hydration never clobbers anything on disk.

use std::collections::HashSet;
use std::path::Path;

use oak_core::{Hash, Manifest, MetadataKey, Repository, Result, SparseCone, SqliteRepository};

use crate::materialize::{apply_manifest, ApplyOpts, DeleteScope, MissingBlobs};
use crate::workdir_lock::WorkdirLock;

/// Blob hashes of `manifest` entries that `in_scope` covers whose content
/// isn't stored locally and that weren't declared lost. Previously restricted hashes are
/// included on purpose: asking again is how a fresh grant is picked up.
/// Sorted, deduplicated.
pub(crate) fn unhydrated_blobs(
    repo: &dyn Repository,
    manifest: &Manifest,
    in_scope: &dyn Fn(&str) -> bool,
) -> Result<Vec<Hash>> {
    let known_lost = super::known_loss::load_known_lost_blobs(repo);
    let mut out = Vec::new();
    for entry in &manifest.entries {
        if !in_scope(&entry.path) || known_lost.contains(entry.blob_hash.as_str()) {
            continue;
        }
        if entry.blob_hash == oak_core::Blob::empty_hash() {
            oak_core::ensure_empty_blob(repo, &entry.blob_hash)?;
            continue;
        }
        if !repo.has_blob(&entry.blob_hash)? {
            out.push(entry.blob_hash.clone());
        }
    }
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out.dedup();
    Ok(out)
}

/// Where to fetch blobs from: the repo's linked remote plus its credential.
/// `None` when the repo has no remote configured.
pub(crate) struct FetchSource {
    pub remote: String,
    pub owner: String,
    pub name: String,
    pub api_key: Option<String>,
}

impl FetchSource {
    /// The linked remote of `repo`, or `None` for a local-only repo.
    pub(crate) fn linked(repo: &SqliteRepository) -> Result<Option<FetchSource>> {
        let Some(remote) = repo
            .get_metadata(MetadataKey::RemoteUrl)?
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| !r.is_empty())
        else {
            return Ok(None);
        };
        let (owner, name) = super::read_repo_identity(repo)?;
        let api_key = super::credentials::effective_token_for_repository(&remote, repo);
        Ok(Some(FetchSource {
            remote,
            owner,
            name,
            api_key,
        }))
    }
}

/// Fetch `hashes`, storing every permitted blob. Returns the hashes the
/// server still withholds (recorded as restricted).
pub(crate) async fn fetch_blobs(
    repo: &SqliteRepository,
    source: &FetchSource,
    hashes: &[Hash],
) -> Result<Vec<Hash>> {
    super::blob_fetch::ensure_blobs_local_allowing_restricted(
        repo,
        &source.remote,
        &source.owner,
        &source.name,
        source.api_key.as_deref(),
        hashes,
    )
    .await
}

/// Write the files of `manifest` that were never written to this working
/// tree but whose content is now stored: in-cone entries absent on disk whose
/// hash is recorded restricted (granted since) or is in `fetched` (just
/// hydrated). Paths that exist on disk — the user's own file, whatever its
/// content — are never touched. Returns the written paths, sorted.
pub(crate) fn write_unwritten_files(
    lock: &WorkdirLock,
    root: &Path,
    repo: &dyn Repository,
    manifest: &Manifest,
    fetched: &HashSet<&str>,
) -> Result<Vec<String>> {
    let restricted = super::restricted::load_restricted_blobs(repo);
    if restricted.is_empty() && fetched.is_empty() {
        return Ok(Vec::new());
    }
    let cone = SparseCone::from_metadata(repo.get_metadata(MetadataKey::SparsePaths)?.as_deref());
    let mut candidates: HashSet<String> = HashSet::new();
    for entry in &manifest.entries {
        let hash = entry.blob_hash.as_str();
        if !(restricted.contains(hash) || fetched.contains(hash))
            || cone.as_ref().is_some_and(|c| !c.covers(&entry.path))
            || !repo.has_blob(&entry.blob_hash)?
        {
            continue;
        }
        match std::fs::symlink_metadata(root.join(&entry.path)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidates.insert(entry.path.clone());
            }
            _ => {}
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let filter = |path: &str| candidates.contains(path);
    apply_manifest(
        lock,
        root,
        repo,
        manifest,
        ApplyOpts {
            delete: DeleteScope::Nothing,
            filter: Some(&filter),
            missing_blobs: MissingBlobs::Skip,
            remove_missing_paths: None,
            clean_empty_dirs: false,
        },
    )?;
    let mut written: Vec<String> = candidates.into_iter().collect();
    written.sort();
    Ok(written)
}
