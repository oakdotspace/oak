//! Local resolution of the active team / project filter.
//!
//! `oak clone --team <slug>` / `oak clone --project <slug>` (and the same
//! flags on `oak mount`) persist the active scope as repo metadata. The
//! commit / restore / switch / pull pipelines read it back through
//! [`active_prefixes`] and use the prefix list to bound their working-
//! tree filtering. Empty list = "whole repo, no filter".
//!
//! The server is the source of truth for which prefixes a team or project
//! covers; the client only learns about them via the `path_prefixes`
//! field of a pull/clone response. We cache the resolved prefixes locally
//! in repo metadata so subsequent commands don't have to re-ask the
//! server every time.

use oak_core::Repository;
use oak_core::{MetadataKey, OakError, Result};

/// Read the active scope's path prefixes, if any. Returns an empty `Vec`
/// when the repo is not scoped (whole-repo behavior).
pub fn active_prefixes(repo: &dyn Repository) -> Result<Vec<String>> {
    let raw = repo
        .get_metadata(MetadataKey::ActivePrefixes)?
        .unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(raw
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Persist the active scope's path prefixes. Empty list clears the filter
/// (the repo reverts to whole-repo behavior).
pub fn set_active_prefixes(repo: &dyn Repository, prefixes: &[String]) -> Result<()> {
    let joined = prefixes
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    repo.set_metadata(MetadataKey::ActivePrefixes, &joined)
}

/// Persist the active team and project slugs that drove the resolved
/// prefixes. Either may be `None`; both `None` clears the scope. The
/// resolved `prefixes` are written through to `ActivePrefixes` so the
/// commit/pull pipelines pick them up without re-asking the server.
pub fn set_active_scope(
    repo: &dyn Repository,
    team: Option<&str>,
    project: Option<&str>,
    prefixes: &[String],
) -> Result<()> {
    repo.set_metadata(MetadataKey::ActiveTeam, team.unwrap_or(""))?;
    repo.set_metadata(MetadataKey::ActiveProject, project.unwrap_or(""))?;
    set_active_prefixes(repo, prefixes)
}

/// Read back the active team slug (or `None`).
pub fn active_team(repo: &dyn Repository) -> Result<Option<String>> {
    Ok(repo
        .get_metadata(MetadataKey::ActiveTeam)?
        .filter(|s| !s.is_empty()))
}

/// Read back the active project slug (or `None`).
pub fn active_project(repo: &dyn Repository) -> Result<Option<String>> {
    Ok(repo
        .get_metadata(MetadataKey::ActiveProject)?
        .filter(|s| !s.is_empty()))
}

/// Reject a list of touched paths against the active prefix filter.
///
/// Used as a defensive check by `oak commit` so a commit under a project
/// scope can't accidentally name an out-of-prefix path (e.g. via
/// `oak mv`). Empty `prefixes` means "no filter" — every path passes.
pub fn reject_out_of_scope<'a>(
    prefixes: &[String],
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    if prefixes.is_empty() {
        return Ok(());
    }
    let offenders: Vec<&str> = paths
        .into_iter()
        .filter(|p| !oak_core::path_in_any_prefix(prefixes, p))
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    let listing = offenders
        .iter()
        .take(5)
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = if offenders.len() > 5 {
        format!("\n  ... and {} more", offenders.len() - 5)
    } else {
        String::new()
    };
    Err(OakError::Server(format!(
        "active scope rejects {} out-of-prefix path(s):\n{}{}\nClone or mount without --team/--project to commit across the full repo.",
        offenders.len(),
        listing,
        suffix
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_out_of_scope_passes_when_prefixes_empty() {
        assert!(reject_out_of_scope(&[], ["any/path"].iter().copied()).is_ok());
    }

    #[test]
    fn reject_out_of_scope_passes_when_all_in_scope() {
        let prefixes = vec!["/payments/".to_string()];
        let result = reject_out_of_scope(
            &prefixes,
            ["payments/auth.rs", "payments/sub/x.rs"].iter().copied(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn reject_out_of_scope_lists_offenders() {
        let prefixes = vec!["/payments/".to_string()];
        let err = reject_out_of_scope(
            &prefixes,
            ["payments/auth.rs", "billing/charge.rs"].iter().copied(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("billing/charge.rs"), "{msg}");
    }
}
