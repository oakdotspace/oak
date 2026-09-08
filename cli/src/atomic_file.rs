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

/// Stream an owner-only file into place without replacing an existing path.
///
/// The destination parent must already exist. The callback writes to a private
/// temporary file in that parent; successful persistence is atomic. If syncing
/// the parent fails after persistence, the returned error explicitly says the
/// destination exists and its crash durability is unconfirmed.
pub fn write_atomic_private_noclobber<T>(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<T>,
) -> Result<T> {
    write_atomic_private_noclobber_with_sync(path, write, sync_parent_dir)
}

fn write_atomic_private_noclobber_with_sync<T>(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<T>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<T> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !parent.is_dir() {
        return Err(OakError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("output parent {} does not exist", parent.display()),
        )));
    }
    let file_name = path.file_name().ok_or_else(|| {
        OakError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no file name",
        ))
    })?;
    let prefix = format!(".{}.tmp-", file_name.to_string_lossy());
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    let value = write(temp.as_file_mut())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|error| OakError::Io(error.error))?;
    sync_parent(parent).map_err(|error| {
        OakError::Io(io::Error::new(
            error.kind(),
            format!(
                "output {} was published, but parent-directory sync failed; durability is unconfirmed: {error}",
                path.display()
            ),
        ))
    })?;
    Ok(value)
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

    #[test]
    fn private_noclobber_streams_and_refuses_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.oakchange");
        write_atomic_private_noclobber(&path, |file| {
            file.write_all(b"archive")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"archive");

        let error = write_atomic_private_noclobber(&path, |file| {
            file.write_all(b"replacement")?;
            Ok(())
        });
        assert!(error.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"archive");
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-")));
    }

    #[test]
    fn private_noclobber_writer_failure_removes_temp_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.oakchange");
        let result = write_atomic_private_noclobber(&path, |file| {
            file.write_all(b"partial")?;
            Err::<(), _>(OakError::Io(io::Error::other("injected writer failure")))
        });
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-")));
    }

    #[test]
    fn private_noclobber_sync_failure_reports_published_file_without_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.oakchange");
        let error = write_atomic_private_noclobber_with_sync(
            &path,
            |file| {
                file.write_all(b"complete")?;
                Ok(())
            },
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();
        assert_eq!(fs::read(&path).unwrap(), b"complete");
        let message = error.to_string();
        assert!(message.contains("was published"));
        assert!(message.contains("durability is unconfirmed"));
    }

    #[test]
    fn private_noclobber_rejects_missing_parent_and_directory_destination() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing/archive.oakchange");
        assert!(write_atomic_private_noclobber(&missing, |_| Ok(())).is_err());
        assert!(!dir.path().join("missing").exists());

        let directory = dir.path().join("destination");
        fs::create_dir(&directory).unwrap();
        assert!(write_atomic_private_noclobber(&directory, |_| Ok(())).is_err());
        assert!(directory.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn private_noclobber_does_not_follow_a_destination_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("archive.oakchange");
        fs::write(&target, b"private").unwrap();
        symlink(&target, &link).unwrap();

        assert!(write_atomic_private_noclobber(&link, |file| {
            file.write_all(b"replacement")?;
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"private");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn private_noclobber_has_one_winner_under_concurrent_creation() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("archive.oakchange"));
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for contents in [b"first".as_slice(), b"second".as_slice()] {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                write_atomic_private_noclobber(&path, |file| {
                    file.write_all(contents)?;
                    Ok(())
                })
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let persisted = fs::read(&*path).unwrap();
        assert!(persisted == b"first" || persisted == b"second");
    }
}
