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

/// Parse an `org/repo` spec into its two parts. Rejects specs without a `/`,
/// or with empty org / repo segments.
pub fn parse_owner_repo(spec: &str) -> Result<(String, String)> {
    let spec = spec.trim();
    let (owner, name) = spec.split_once('/').ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "Expected org/repo format (e.g. oak/testrepo), got '{spec}'"
        ))
    })?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty() || name.is_empty() {
        return Err(OakError::InvalidArgument(format!(
            "Invalid org/repo spec '{spec}': both segments are required"
        )));
    }
    if name.contains('/') {
        return Err(OakError::InvalidArgument(format!(
            "Invalid org/repo spec '{spec}': repo name cannot contain '/'"
        )));
    }
    validate_repo_spec_segment(spec, "org", owner)?;
    validate_repo_spec_segment(spec, "repo", name)?;
    Ok((owner.to_string(), name.to_string()))
}

fn validate_repo_spec_segment(spec: &str, label: &str, value: &str) -> Result<()> {
    if value == "." || value == ".." {
        return Err(OakError::InvalidArgument(format!(
            "Invalid org/repo spec '{spec}': {label} cannot be '.' or '..'"
        )));
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(OakError::InvalidArgument(format!(
            "Invalid org/repo spec '{spec}': {label} cannot contain whitespace, control characters, or backslashes"
        )));
    }
    Ok(())
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
        OakError::Config(format!(
            "This repository is not linked to an Oak remote. Run `{}` to link and publish it, or re-clone with `oak clone <org>/<repo>`.",
            push::PUSH_REPO_PLACEHOLDER_COMMAND
        ))
    })?;
    let name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
        OakError::Config(format!(
            "This repository is missing remote repo metadata. Run `{}` to repair and publish it, or re-clone with `oak clone <org>/<repo>`.",
            push::PUSH_REPO_PLACEHOLDER_COMMAND
        ))
    })?;
    Ok((owner, name))
}

/// The repo's stored remote origin (`MetadataKey::RemoteUrl`), if any. Used
/// by commands that read the remote from metadata (fetch, merge) to name
/// the old origin when following a host move.
pub(crate) fn stored_remote_url(path: &std::path::Path) -> Result<Option<String>> {
    let ctx = crate::resolve::resolve(path)?;
    ctx.open()?.get_metadata(MetadataKey::RemoteUrl)
}

/// React to an [`OakError::RemoteMoved`] whose target is a trusted Oak host
/// (the caller checks `crate::http::is_trusted_origin` before calling this):
/// persist `origin` as the repo's remote, carry the old host's stored login
/// over to the new one, and tell the user what's happening. The caller then
/// retries the operation once against `origin`.
pub(crate) fn follow_remote_move(
    path: &std::path::Path,
    old_remote: &str,
    origin: &str,
) -> Result<()> {
    crate::output::info(&format!(
        "Remote {old_remote} has moved to {origin} — updating this repo's remote and retrying"
    ));
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    repo.set_metadata(MetadataKey::RemoteUrl, origin)?;
    if credentials::migrate_server_credential(old_remote, origin)? {
        crate::output::info(&format!("Carried your {old_remote} login over to {origin}"));
    }
    Ok(())
}

/// Web UI URLs share the configured remote origin with API URLs; the API lives
/// under `/api`, while user-facing repo pages live at `/<owner>/<repo>`.
pub fn repo_web_url(remote: &str, repo_path: &str) -> String {
    format!(
        "{}/{}",
        remote.trim_end_matches('/'),
        repo_path.trim_matches('/')
    )
}

pub fn branch_web_url(remote: &str, repo_path: &str, branch: &str) -> String {
    format!(
        "{}/branches/{}",
        repo_web_url(remote, repo_path),
        branch_api_segment(branch)
    )
}

pub fn branch_api_segment(branch: &str) -> String {
    urlencoding::encode(branch).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{branch_web_url, parse_owner_repo, repo_web_url};

    #[test]
    fn repo_web_url_trims_slashes() {
        assert_eq!(
            repo_web_url("https://oak.space/", "/acme/website/"),
            "https://oak.space/acme/website"
        );
    }

    #[test]
    fn branch_web_url_encodes_branch_name() {
        assert_eq!(
            branch_web_url("https://oak.space/", "/acme/website/", "feature/foo bar"),
            "https://oak.space/acme/website/branches/feature%2Ffoo%20bar"
        );
    }

    #[test]
    fn parse_owner_repo_accepts_simple_spec() {
        assert_eq!(
            parse_owner_repo("oak/benchmarks").unwrap(),
            ("oak".to_string(), "benchmarks".to_string())
        );
    }

    #[test]
    fn parse_owner_repo_rejects_remote_unsafe_segments() {
        for spec in [
            "oak/repo name",
            "oak/repo\tname",
            "oak/repo\nname",
            "oak/repo\u{1b}name",
            "oak/repo\u{7f}name",
            "oak/repo\\name",
            "oak/repo\0name",
            "oak/.",
            "oak/..",
            "./repo",
            "../repo",
        ] {
            let err = parse_owner_repo(spec).unwrap_err();
            assert!(
                matches!(err, oak_core::OakError::InvalidArgument(_)),
                "{spec:?} should be InvalidArgument, got {err:?}"
            );
        }
    }
}

pub mod archive;
pub mod blob_fetch;
pub mod branch;
pub mod checkout;
pub mod ci_fetch;
pub mod commit;
pub mod conflict;
pub mod credentials;
pub mod diff;
pub mod export;
pub mod feedback;
pub mod fetch;
pub mod finish;
pub mod git_clone;
pub mod hash;
pub mod hooks;
pub mod init;
pub mod log;
pub mod login;
pub mod logout;
pub mod maintenance;
pub mod merge;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub mod mount;
pub mod open;
pub mod pull;
pub mod push;
pub mod release;
pub mod repo;
pub mod reset;
pub mod restore;
pub mod review;
pub mod serve;
pub mod site;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub mod spaces;
pub mod sparse;
pub mod split;
pub mod status;
pub mod switch;
pub mod sync;
pub mod triage;
pub mod upgrade;
pub mod version_check;
pub mod whoami;
