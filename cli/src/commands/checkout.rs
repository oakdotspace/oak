//! `oak checkout <hash>` — detach HEAD at a specific commit.
//!
//! Thin wrapper over `oak switch --detach`. The two are equivalent; the
//! `checkout` spelling exists because git users reach for it by reflex.

use std::path::Path;

use oak_core::Repository;
use oak_core::{Hash, OakError, Result};

use super::switch;

/// Check out (detach HEAD at) a specific commit.
///
/// Accepts either:
///   - a full commit hash (40 or 64 hex chars), or
///   - a short hash prefix (≥ 4 chars), which is resolved against the
///     local commits table.
pub fn run(path: &Path, reference: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let full_hash = resolve_commit_ref(repo.as_ref(), reference)?;
    drop(repo);

    switch::run(path, Some(full_hash.as_str()), true)
}

/// Resolve a hash-or-prefix to a full commit hash. Errors if no commit
/// matches or if the prefix is ambiguous.
pub fn resolve_commit_ref(repo: &dyn Repository, reference: &str) -> Result<Hash> {
    if reference.is_empty() {
        return Err(OakError::InvalidHash(reference.to_string()));
    }

    // Full hash: try directly.
    if let Ok(h) = Hash::from_hex(reference) {
        if repo.get_commit(&h)?.is_some() {
            return Ok(h);
        }
        return Err(OakError::CommitNotFound(reference.to_string()));
    }

    // Short prefix: scan local commits for a unique match.
    if reference.len() < 4 {
        return Err(OakError::InvalidHash(format!(
            "'{reference}' too short — use at least 4 hex chars"
        )));
    }
    if !reference.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(OakError::InvalidHash(reference.to_string()));
    }

    let mut matches: Vec<Hash> = repo
        .get_all_commits()?
        .into_iter()
        .map(|c| c.hash)
        .filter(|h| h.as_str().starts_with(reference))
        .collect();

    match matches.len() {
        0 => Err(OakError::CommitNotFound(reference.to_string())),
        1 => Ok(matches.pop().unwrap()),
        n => Err(OakError::InvalidHash(format!(
            "'{reference}' matches {n} commits — use a longer prefix"
        ))),
    }
}
