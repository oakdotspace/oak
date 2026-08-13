use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{OakError, Result};

/// Atomically replace `path` with `contents`.
///
/// The temp file is created in the destination directory, fully written and
/// synced, then renamed into place. On Unix we also sync the parent directory
/// so the rename itself is durable across a crash.
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    write_atomic_impl(path, contents, false)
}

/// Atomically replace `path` with `contents`, creating the replacement file
/// with owner-only permissions before any contents are written on Unix.
pub fn write_atomic_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    write_atomic_impl(path, contents, true)
}

fn write_atomic_impl(path: &Path, contents: impl AsRef<[u8]>, private: bool) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        OakError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no parent",
        ))
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path.file_name().ok_or_else(|| {
        OakError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no file name",
        ))
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp_name = format!(
        ".{}.tmp-{}-{nonce}",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);

    let write_result = (|| -> Result<()> {
        let mut file = create_temp_file(&tmp_path, private)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        sync_parent_dir(parent)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

fn create_temp_file(path: &Path, private: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        return Ok(file);
    }
    let _ = private;
    options.open(path)
}

#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_and_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");

        write_atomic(&path, "first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");

        write_atomic(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write should not leave temp files behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");

        write_atomic_private(&path, "secret").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_replaces_world_readable_file_with_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic_private(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
