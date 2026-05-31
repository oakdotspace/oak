use std::path::Path;

use oak_core::Result;

/// Print the current HEAD commit hash
pub fn run(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = match &branch_name {
        Some(name) => repo.get_branch_head(name)?,
        None => repo.get_head()?,
    };

    match head {
        Some(hash) => {
            println!("{}", hash.short());
            Ok(())
        }
        None => Err(oak_core::OakError::RepoNotFound),
    }
}
