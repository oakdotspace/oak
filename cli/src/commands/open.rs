use oak_core::{BranchStatus, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use std::path::Path;

use crate::output;

pub fn run(path: &Path) -> Result<()> {
    let url = match crate::resolve::resolve(path) {
        Ok(ctx) => {
            let repo = SqliteRepository::open(&ctx.db_path()?)?;
            let (owner, name) = crate::commands::read_repo_identity(&repo)?;

            let remote_url = repo
                .get_metadata(MetadataKey::RemoteUrl)?
                .or_else(|| std::env::var("OAK_REMOTE").ok())
                .unwrap_or_else(|| "https://oakvcs.com".to_string());

            let base = format!("{}/{}/{}", remote_url.trim_end_matches('/'), owner, name);
            // Only deep-link to the branch page when the current branch is
            // actually viewable on the server. Two cases would otherwise 404:
            //   * a closed branch (just merged onto main), and
            //   * a fresh personal branch with no commits of its own — the
            //     state `oak merge` leaves you in, which has never been pushed.
            // In both cases the branch page has nothing useful to show, so fall
            // back to the repo home.
            match repo.get_current_branch_name().ok().flatten() {
                Some(branch) if !branch.is_empty() && branch_is_viewable(&repo, &branch) => {
                    format!("{}/branches/{}", base, urlencoding::encode(&branch))
                }
                _ => base,
            }
        }
        Err(OakError::RepoNotFound) => "https://oakvcs.com".to_string(),
        Err(e) => return Err(e),
    };

    open::that(&url).map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    output::success(&format!("Opened {url}"));
    Ok(())
}

/// Whether the branch page on the server is worth linking to. Returns `false`
/// for branches that would 404 (or show nothing): a closed/merged branch, or a
/// branch with no commits of its own (never pushed, e.g. the fresh personal
/// branch `oak merge` switches you onto). Any local read error is treated as
/// non-viewable so we fall back to the repo home rather than a broken link.
fn branch_is_viewable(repo: &SqliteRepository, branch: &str) -> bool {
    match repo.get_branch(branch) {
        Ok(Some(b)) if b.status == BranchStatus::Closed => return false,
        Ok(Some(_)) => {}
        // Unknown branch / lookup error: don't deep-link.
        _ => return false,
    }
    matches!(repo.get_commits_for_branch(branch), Ok(commits) if !commits.is_empty())
}
