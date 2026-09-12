//! `oak ci-fetch` — token-scoped fetch of a single commit's working tree.
//!
//! This is the source-fetch primitive Oak's native CI uses inside the execution
//! sandbox. The control plane (oak-web) mints a short-lived, repo-scoped fetch
//! token at dispatch and passes it to the backend; the backend runs
//!
//! ```text
//! oak ci-fetch --remote https://oak.space --commit <hash> --token <t> /workspace
//! ```
//!
//! which downloads the commit's tree from `GET /api/ci/fetch` (a zip) and
//! unpacks it into the destination, preserving executable bits and symlinks.
//! It is deliberately NOT a full `oak clone`: no login, no local repo, no
//! history — just the exact tree to build against. The token IS the
//! authorization, so this needs no credentials and is safe to run unattended.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use oak_core::{OakError, Result};

use crate::output;

/// Wrap a filesystem failure with path context. `OakError::Io` carries a
/// `std::io::Error`, so build one that keeps the message.
fn io_err(msg: String) -> OakError {
    OakError::Io(std::io::Error::other(msg))
}

/// Fetch `commit`'s tree from `remote` using `token` and unpack into `dest`.
pub async fn run(remote: &str, commit: &str, token: &str, dest: &Path) -> Result<()> {
    let base = remote.trim_end_matches('/');
    let url = format!("{base}/api/ci/fetch");

    let client = crate::http::api_client();
    let resp = client
        .get(&url)
        .query(&[("t", token), ("commit", commit)])
        .send()
        .await
        .map_err(|e| OakError::Server(format!("ci-fetch request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let msg = crate::http::json_error_message(&body).unwrap_or(body);
        return Err(OakError::Server(format!(
            "ci-fetch {commit} -> {status}: {msg}"
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| OakError::Server(format!("ci-fetch download failed: {e}")))?;

    std::fs::create_dir_all(dest).map_err(|e| io_err(format!("create {}: {e}", dest.display())))?;
    let count = unpack_zip(&bytes, dest)?;
    output::success(&format!(
        "Fetched {} file{} from {} into {}",
        count,
        if count == 1 { "" } else { "s" },
        &commit[..commit.len().min(12)],
        dest.display()
    ));
    Ok(())
}

/// Unpack a zip archive into `dest`, preserving exec bits + symlinks. Returns
/// the number of entries written. Rejects entries whose path escapes `dest`
/// (zip-slip) — the archive comes from a trusted server, but the cost of the
/// guard is nil and the failure mode is severe.
fn unpack_zip(bytes: &[u8], dest: &Path) -> Result<usize> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| OakError::Server(format!("ci-fetch: invalid archive: {e}")))?;
    let mut written = 0usize;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| OakError::Server(format!("ci-fetch: read entry {i}: {e}")))?;
        let Some(rel) = file.enclosed_name() else {
            return Err(OakError::Server(format!(
                "ci-fetch: unsafe path in archive: {}",
                file.name()
            )));
        };
        let out = safe_join(dest, &rel)?;
        let mode = file.unix_mode().unwrap_or(0o644);

        if file.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| io_err(format!("mkdir {}: {e}", out.display())))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io_err(format!("mkdir {}: {e}", parent.display())))?;
        }

        // Symlinks are stored with the S_IFLNK type bits; the content is the
        // link target.
        if mode & 0o170000 == 0o120000 {
            let mut target = String::new();
            file.read_to_string(&mut target)
                .map_err(|e| io_err(format!("read symlink {}: {e}", out.display())))?;
            write_symlink(&out, &target)?;
            written += 1;
            continue;
        }

        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)
            .map_err(|e| io_err(format!("read {}: {e}", out.display())))?;
        std::fs::write(&out, &buf).map_err(|e| io_err(format!("write {}: {e}", out.display())))?;
        set_mode(&out, mode);
        written += 1;
    }
    Ok(written)
}

/// Join `rel` onto `dest`, erroring if the result escapes `dest`. `rel` is
/// already an `enclosed_name()` (no `..`, not absolute), so this is defense in
/// depth.
fn safe_join(dest: &Path, rel: &Path) -> Result<PathBuf> {
    let out = dest.join(rel);
    if !out.starts_with(dest) {
        return Err(OakError::Server(format!(
            "ci-fetch: path escapes destination: {}",
            rel.display()
        )));
    }
    Ok(out)
}

#[cfg(unix)]
fn write_symlink(out: &Path, target: &str) -> Result<()> {
    // Replace an existing entry so re-fetch is idempotent.
    let _ = std::fs::remove_file(out);
    std::os::unix::fs::symlink(target, out)
        .map_err(|e| io_err(format!("symlink {}: {e}", out.display())))
}

#[cfg(not(unix))]
fn write_symlink(out: &Path, target: &str) -> Result<()> {
    // No symlink type on this platform — write the target as file content so the
    // tree is at least complete.
    std::fs::write(out, target).map_err(|e| io_err(format!("write {}: {e}", out.display())))
}

#[cfg(unix)]
fn set_mode(out: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    // Keep only the permission bits; clamp to sane file perms.
    let perms = std::fs::Permissions::from_mode(mode & 0o777);
    let _ = std::fs::set_permissions(out, perms);
}

#[cfg(not(unix))]
fn set_mode(_out: &Path, _mode: u32) {}
