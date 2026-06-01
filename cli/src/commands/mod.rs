use oak_core::{MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

/// RAII guard that re-enables SQLite foreign-key enforcement on drop.
///
/// Used by the clone and pull paths: the server's PostgreSQL schema doesn't
/// enforce `parent_branch` / `parent_hash` FKs, so a pull may legitimately
/// reference parents that aren't in the result set (deleted parent branches,
/// commits whose timestamps don't match topology after a rebase, etc.). We
/// disable enforcement for the bulk import and let this guard re-enable it on
/// every exit path so subsequent normal operations stay validated.
pub struct FkGuard<'a> {
    pub repo: &'a SqliteRepository,
}

impl Drop for FkGuard<'_> {
    fn drop(&mut self) {
        let _ = self.repo.set_foreign_keys(true);
    }
}

/// RAII guard around a relaxed-durability bulk-import transaction (see
/// [`SqliteRepository::bulk_begin`]). Opens the batched transaction on
/// construction and rolls it back on drop unless [`BulkTxn::commit`] ran
/// first, so a mid-import error (a failed download, a bad wire object) can't
/// leave a dangling transaction behind. Used by the `oak clone` / `oak pull`
/// ingest, where the per-object writes would otherwise each be their own
/// fsync'd commit.
pub struct BulkTxn<'a> {
    repo: &'a SqliteRepository,
    armed: bool,
}

impl<'a> BulkTxn<'a> {
    /// Open the bulk transaction.
    pub fn begin(repo: &'a SqliteRepository) -> Result<Self> {
        repo.bulk_begin()?;
        Ok(Self { repo, armed: true })
    }

    /// Commit the current batch and start the next one, to bound WAL growth
    /// on a large import. Cheap — see [`SqliteRepository::bulk_flush`].
    pub fn flush(&self) -> Result<()> {
        self.repo.bulk_flush()
    }

    /// Commit the transaction and restore full durability. Consumes the guard
    /// so a clean path doesn't roll back on drop.
    pub fn commit(mut self) -> Result<()> {
        self.repo.bulk_commit()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for BulkTxn<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.repo.bulk_rollback();
        }
    }
}

/// Parse an `owner/name` spec into its two parts. Rejects specs without a `/`,
/// or with empty owner / name segments.
pub fn parse_owner_repo(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim();
    let (owner, name) = spec.split_once('/').ok_or_else(|| {
        OakError::Server(format!(
            "Expected owner/repo format (e.g. oak/testrepo), got '{spec}'"
        ))
    })?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty() || name.is_empty() {
        return Err(OakError::Server(format!(
            "Invalid owner/repo spec '{spec}': both segments are required"
        )));
    }
    if name.contains('/') {
        return Err(OakError::Server(format!(
            "Invalid owner/repo spec '{spec}': repo name cannot contain '/'"
        )));
    }
    Ok((owner.to_string(), name.to_string()))
}

/// Sort branch-like items so that each parent appears before its children.
/// Branches whose `parent_branch` is missing from the input are placed first
/// among their group so FK-enforcing stores can accept them.
pub fn sort_branches_topologically<T, F, G>(items: Vec<T>, name_of: F, parent_of: G) -> Vec<T>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> Option<&str>,
{
    use std::collections::HashSet;
    let mut result: Vec<T> = Vec::with_capacity(items.len());
    let mut placed: HashSet<String> = HashSet::new();
    let mut remaining: Vec<T> = items;
    while !remaining.is_empty() {
        let mut progress = false;
        let mut i = 0;
        while i < remaining.len() {
            let ready = match parent_of(&remaining[i]) {
                None => true,
                Some(p) => placed.contains(p),
            };
            if ready {
                let item = remaining.swap_remove(i);
                placed.insert(name_of(&item).to_string());
                result.push(item);
                progress = true;
            } else {
                i += 1;
            }
        }
        if !progress {
            result.extend(remaining);
            break;
        }
    }
    result
}

/// Read `(owner, name)` from repo metadata. `RepoOwner` is required for all
/// server-side operations under the new URL scheme; missing it means the repo
/// was created before this version and needs to be re-cloned.
pub fn read_repo_identity(repo: &dyn Repository) -> Result<(String, String)> {
    let owner = repo.get_metadata(MetadataKey::RepoOwner)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing owner; re-clone with `oak clone <owner>/<repo>`"
                .to_string(),
        )
    })?;
    let name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Server(
            "Repository metadata missing repo name; re-clone with `oak clone <owner>/<repo>`"
                .to_string(),
        )
    })?;
    Ok((owner, name))
}

pub mod archive;
pub mod blob_fetch;
pub mod branch;
pub mod checkout;
pub mod commit;
pub mod credentials;
pub mod diff;
pub mod export;
pub mod fetch;
pub mod git_clone;
pub mod hash;
pub mod histedit;
pub mod hooks;
pub mod init;
pub mod log;
pub mod login;
pub mod merge;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub mod mount;
pub mod open;
pub mod project;
pub mod prompt;
pub mod pull;
pub mod push;
pub mod query;
pub mod release;
pub mod repo;
pub mod reset;
pub mod restore;
pub mod serve;
pub mod site;
pub mod spaces;
pub mod status;
pub mod switch;
pub mod sync;
pub mod tag;
pub mod tree;
pub mod upgrade;
pub mod version_check;
