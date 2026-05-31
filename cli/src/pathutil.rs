//! Helpers for translating user-supplied paths into repo-root-relative paths.
//!
//! Oak commands accept paths from the CLI that may be:
//! - absolute, or
//! - relative to the current working directory (which may be a subdirectory
//!   of the repo, since `resolve()` walks up to find the repo root).
//!
//! Internally, almost everything wants the path as repo-root-relative. These
//! helpers do that translation while also handling the macOS `/tmp` →
//! `/private/tmp` symlink (resolved via `canonicalize`) and paths that don't
//! exist yet (canonicalize the parent and re-attach the leaf).

use std::path::{Path, PathBuf};

/// Resolve `input` against `cwd`, then strip `work_tree` to yield a
/// repo-root-relative path. Falls back to a best-effort `PathBuf` if the
/// input lies outside the repo — callers that need to reject out-of-repo
/// inputs should use [`repo_relative_str`] instead.
pub fn repo_relative(cwd: &Path, work_tree: &Path, input: &Path) -> PathBuf {
    let canonical = canonicalize_lenient(&absolutize(cwd, input));
    canonical
        .strip_prefix(work_tree)
        .map(|r| r.to_path_buf())
        .unwrap_or(canonical)
}

/// Like [`repo_relative`] but returns the path as a forward-slash string,
/// erroring if the path is not inside `work_tree`. Use this for callers that
/// send the path off-machine (e.g., the file-locks API) where an out-of-repo
/// path is meaningless.
pub fn repo_relative_str(cwd: &Path, work_tree: &Path, input: &str) -> oak_core::Result<String> {
    let abs = absolutize(cwd, Path::new(input));
    let canonical = canonicalize_lenient(&abs);
    let stripped = canonical.strip_prefix(work_tree).map_err(|_| {
        oak_core::OakError::Config(format!("path '{input}' is not inside the repository"))
    })?;
    Ok(stripped.to_string_lossy().replace('\\', "/"))
}

fn absolutize(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Canonicalize `p` if it exists; otherwise canonicalize its parent and
/// re-attach the file name. This lets us resolve symlinks (notably macOS's
/// `/tmp` → `/private/tmp`) for paths that point at not-yet-created files
/// (e.g., the destination of `oak mv`).
fn canonicalize_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if let (Some(parent), Some(leaf)) = (p.parent(), p.file_name()) {
        if let Ok(c) = std::fs::canonicalize(parent) {
            return c.join(leaf);
        }
    }
    p.to_path_buf()
}
