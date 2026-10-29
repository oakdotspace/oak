#[cfg(unix)]
use std::fs;
use std::path::Path;

use oak_core::FileMode;

/// Apply file permissions after writing a file to disk.
#[cfg(unix)]
pub fn apply_file_permissions(file_path: &Path, mode: FileMode) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let perm_mode = match mode {
        FileMode::Executable => 0o755,
        _ => 0o644,
    };

    let mut perms = fs::metadata(file_path)?.permissions();
    perms.set_mode(perm_mode);
    fs::set_permissions(file_path, perms)
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn apply_file_permissions(_file_path: &Path, _mode: FileMode) -> std::io::Result<()> {
    Ok(())
}

/// The on-disk mode of an existing path, as the working-dir walk (and thus
/// `oak status`) sees it: a symlink is `Symlink` (never followed), otherwise the
/// unix `0o111` execute bits map to `Executable` and everything else to
/// `Regular`. This mirrors the walk in `commands::commit` exactly so `oak reset`
/// / `oak restore` change-detection agrees with `oak status` about mode. Returns
/// `None` if the path can't be stat'd.
#[cfg(unix)]
pub fn current_file_mode(file_path: &Path) -> Option<FileMode> {
    use std::os::unix::fs::PermissionsExt;

    // lstat first: a symlink is its own mode, not its target's.
    if fs::symlink_metadata(file_path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return Some(FileMode::Symlink);
    }
    let md = fs::metadata(file_path).ok()?;
    if md.permissions().mode() & 0o111 != 0 {
        Some(FileMode::Executable)
    } else {
        Some(FileMode::Regular)
    }
}

/// Mode isn't represented in non-Unix working trees, so nothing is ever
/// reported as a mode change there (mirrors the walk, which always emits
/// `Regular`).
#[cfg(not(unix))]
pub fn current_file_mode(_file_path: &Path) -> Option<FileMode> {
    None
}
