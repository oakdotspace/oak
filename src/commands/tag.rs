use std::path::Path;

use oak_core::{Hash, OakError, Result};

use crate::output;

/// Create a tag pointing to HEAD or a specific commit
pub fn create(path: &Path, name: &str, commit_hash: Option<&str>) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let hash = if let Some(h) = commit_hash {
        let hash = Hash(h.to_string());
        if !repo.has_commit(&hash)? {
            return Err(OakError::CommitNotFound(h.to_string()));
        }
        hash
    } else {
        let branch_name = repo
            .get_current_branch_name()?
            .ok_or(OakError::BranchNotFound("no current branch".to_string()))?;
        repo.get_branch_head(&branch_name)?
            .ok_or(OakError::NoCommits)?
    };

    repo.create_tag(name, &hash)?;

    output::success(&format!(
        "Created tag '{}{}{}' at {}{}{}",
        output::colors::BOLD,
        name,
        output::colors::RESET,
        output::colors::CYAN,
        hash.short(),
        output::colors::RESET,
    ));

    Ok(())
}

/// List all tags
pub fn list(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let tags = repo.list_tags()?;

    if tags.is_empty() {
        output::info("No tags");
        return Ok(());
    }

    for tag in &tags {
        output::print_line(&format!(
            "{}{}{:<20}{} {}{}{} {}({}){}",
            output::colors::MAGENTA,
            output::colors::BOLD,
            tag.name,
            output::colors::RESET,
            output::colors::CYAN,
            tag.commit_hash.short(),
            output::colors::RESET,
            output::colors::DIM,
            tag.created_at.format("%Y-%m-%d %H:%M:%S"),
            output::colors::RESET,
        ));
    }

    Ok(())
}

/// Delete a tag
pub fn delete(path: &Path, name: &str) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    repo.delete_tag(name)?;

    output::success(&format!("Deleted tag '{name}'"));

    Ok(())
}
