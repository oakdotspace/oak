use std::path::Path;

use oak_core::{OakError, Result};

use crate::commands::commit::resolve_effective_head;
use crate::output;

/// Print the current HEAD commit hash to stdout.
///
/// Output is a single bare line — no color, no label — so it stays
/// machine-parseable for piping (e.g. `oak hash | head -c 8` to derive a short
/// build SHA).
///
/// HEAD is resolved exactly the way `oak status` / `oak commit` see it: prefer
/// the current branch's effective head (walking parent branches for a branch
/// with no commits of its own), falling back to the legacy detached-HEAD
/// pointer. Resolving the same way guarantees `oak hash` never disagrees with
/// what a commit would be parented onto.
pub fn run(path: &Path) -> Result<()> {
    let head = current_head(path)?;
    output::print_line(head.as_str());
    Ok(())
}

fn current_head(path: &Path) -> Result<oak_core::Hash> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let head = match repo.get_current_branch_name().ok().flatten() {
        Some(name) => resolve_effective_head(repo.as_ref(), &name)?,
        None => repo.get_head()?,
    };

    head.ok_or(OakError::NoCommits)
}
