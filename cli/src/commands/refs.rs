//! `oak refs inspect` (fb-460): every local ref from one pinned SQLite read
//! transaction, with disagreements flagged. Read-only; no network, no
//! hydration, no migration.
use oak_core::{sqlite::file_inspect::InspectionStatus, OakError, Result, SqliteRepository};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Returns whether the effective HEAD resolved to a present commit with no
/// disagreements (the exit-0 condition).
pub fn inspect(cwd: &Path, max_branches: u64, json: bool) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if super::mount::mount_dest_for(cwd)?.is_some() {
        return Err(OakError::Unsupported {
            backend: "mount",
            op: "refs inspect",
        });
    }
    let ctx = crate::resolve::resolve(cwd)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "refs inspect",
        });
    }
    let repo = SqliteRepository::open_read_only(&ctx.db_path()?)?;
    let refs = repo.inspect_refs(max_branches)?;
    let consistent =
        refs.effective_head.status == InspectionStatus::Verified && refs.disagreements.is_empty();

    #[derive(Serialize)]
    struct Output {
        schema_version: u32,
        kind: &'static str,
        repository_root: PathBuf,
        backend: &'static str,
        consistent: bool,
        refs: oak_core::sqlite::refs_inspect::RefsInspection,
        recommended_next_commands: Vec<String>,
    }
    let mut next = Vec::new();
    if refs.branches_truncated {
        next.push(format!(
            "oak refs inspect --json --max-branches {}",
            refs.branch_count
                .min(oak_core::sqlite::refs_inspect::MAX_REFS_MAX_BRANCHES)
        ));
    }
    if let Some(commit) = refs.effective_head.commit.as_deref() {
        if refs.effective_head.commit_present == Some(true) {
            next.push(format!("oak tree inspect --at {commit} --json"));
        }
    }
    if json {
        crate::output::print_json(&Output {
            schema_version: 1,
            kind: "refs_inspection",
            repository_root: ctx.work_tree,
            backend: "sqlite",
            consistent,
            refs,
            recommended_next_commands: next,
        })?;
    } else {
        crate::output::print_line(&format!(
            "current branch: {}",
            refs.current_branch.as_deref().unwrap_or("(detached)")
        ));
        crate::output::print_line(&format!(
            "effective HEAD: {} ({})",
            refs.effective_head.commit.as_deref().unwrap_or("(none)"),
            refs.effective_head.source
        ));
        crate::output::print_line(&format!(
            "legacy metadata.head: {}",
            refs.legacy_head.as_deref().unwrap_or("(none)")
        ));
        for branch in &refs.branches {
            crate::output::print_line(&format!(
                "{} {} {}{}",
                if branch.current { "*" } else { " " },
                branch.name,
                branch.effective_head.as_deref().unwrap_or("(no head)"),
                branch
                    .inherited_from
                    .as_deref()
                    .map(|from| format!(" (inherited from {from})"))
                    .unwrap_or_default()
            ));
        }
        for disagreement in &refs.disagreements {
            crate::output::print_line(&format!(
                "disagreement: {}{}: {}",
                disagreement.kind,
                disagreement
                    .branch
                    .as_deref()
                    .map(|branch| format!(" [{branch}]"))
                    .unwrap_or_default(),
                disagreement.detail
            ));
        }
    }
    Ok(consistent)
}
