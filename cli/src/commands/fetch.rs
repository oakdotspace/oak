use std::path::Path;

use oak_core::Result;
use oak_core::{Repository, SqliteRepository};

use crate::output;

/// Refresh the local copy of `main` (and any other server-only parent
/// branches) without touching the working tree or running a merge.
///
/// This is the read-only counterpart to `oak pull`: it downloads the
/// parent branch's HEAD commit, manifest, and any missing blobs into
/// local storage so subsequent offline operations (status, diff, log,
/// merge previews) see the current server state. The user's current
/// branch and working tree are left untouched — useful when you want
/// "what's new on main?" without applying it.
///
/// `main` is the only branch fetched today because it's the only branch
/// that lives server-only and gets out of sync with the local DB. Other
/// branches you've already pulled stay in sync via normal `oak pull`.
pub async fn run(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let _lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open(&db_path)?;

    let before = repo.get_branch_head("main").ok().flatten();
    let after = super::sync::fetch_parent_from_server(&repo, "main").await?;

    match (before, after) {
        (Some(b), Some(a)) if b == a => output::info("Already up to date"),
        (_, Some(a)) => output::success(&format!("Fetched main → {}", a.short())),
        (_, None) => output::info("Remote has no commits on main yet"),
    }
    Ok(())
}
