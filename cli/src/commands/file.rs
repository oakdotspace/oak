//! Local committed-file evidence, never working-copy content or remote hydration.
use oak_core::{sqlite::file_inspect::InspectionStatus, OakError, Result, SqliteRepository};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn inspect(cwd: &Path, revision: &str, path: &str, max_bytes: u64, json: bool) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if super::mount::mount_dest_for(cwd)?.is_some() {
        return Err(OakError::Unsupported {
            backend: "mount",
            op: "file inspect",
        });
    }
    let ctx = crate::resolve::resolve(cwd)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "file inspect",
        });
    }
    let repo = SqliteRepository::open_read_only(&ctx.db_path()?)?;
    let evidence = repo.inspect_pinned_file(revision, path, max_bytes)?;
    let verified = evidence.status == InspectionStatus::Verified;
    #[derive(Serialize)]
    struct Output {
        schema_version: u32,
        kind: &'static str,
        repository_root: PathBuf,
        backend: &'static str,
        evidence: oak_core::sqlite::file_inspect::FileInspection,
        recommended_next_commands: Vec<&'static str>,
    }
    if json {
        crate::output::print_json(&Output {
            schema_version: 1,
            kind: "file_inspection",
            repository_root: ctx.work_tree,
            backend: "sqlite",
            evidence,
            recommended_next_commands: vec!["oak file inspect --help"],
        })?;
    } else {
        crate::output::print_line(&format!(
            "{:?}: {} at {}",
            evidence.status,
            evidence.path,
            evidence.commit.as_deref().unwrap_or(revision)
        ));
        if let Some(reason) = evidence.reason {
            crate::output::print_line(&reason);
        }
    }
    Ok(verified)
}
