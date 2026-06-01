//! macOS FSKit backend for `oak mount`.
//!
//! Unlike the Linux FUSE backend, the filesystem does **not** run in this
//! process. macOS loads the `OakFS` FSKit extension (`OakFS.appex`, Swift,
//! see `macos/OakFS/`) into its own sandboxed process. That extension can't
//! make oak's network calls or touch `~/.oak`, so it forwards every VFS
//! operation to this daemon, which owns [`MountCore`] and answers.
//!
//! This module is the daemon half:
//!   - [`protocol`] — the request/reply wire contract.
//!   - [`server`]   — [`server::MountServer`], dispatch over `MountCore`.
//!   - [`ipc`]      — length-prefixed framing + the local socket server.
//!
//! Lifecycle ([`FskitMount`]): start the IPC server, then ask the kernel to
//! mount the `OakFS` volume (`/sbin/mount`, which the daemon may call because
//! it is **not** sandboxed). On stop, unmount and tear the server down.
//!
//! The advantage over FUSE: **no kernel extension**. The user installs and
//! enables the signed `Oak Mounter` app once; there is no macFUSE kext, no
//! reduced-security boot, and it survives OS upgrades.
//!
//! ### Remaining on-device integration
//! The extension↔daemon transport is XPC in production (the sandbox forbids
//! the extension from opening the unix socket directly). `ipc.rs` implements
//! the framed protocol over a unix socket for development and tests; the
//! production build bridges XPC to it. Building and signing the extension
//! requires Xcode and the `com.apple.developer.file-system.fskit` entitlement.
//! See `macos/OakFS/README.md`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use oak_core::{OakError, Result};

use super::core::MountCore;

pub mod ipc;
pub mod protocol;
pub mod server;

/// The filesystem short-name registered by the FSKit extension's Info.plist
/// (`FSShortName`). `mount -t OakFS …` selects it.
pub const FS_SHORT_NAME: &str = "OakFS";

/// A live FSKit mount: the daemon-side IPC server plus the mounted volume.
pub struct FskitMount {
    ipc: ipc::IpcServer,
    mount_point: PathBuf,
}

/// Path to the daemon's IPC socket for a given mount state dir.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("fskit.sock")
}

impl FskitMount {
    /// Start the IPC server and mount the `OakFS` volume at `mount_point`.
    ///
    /// `state_dir` is where the IPC socket lives. The mount id (from the
    /// config inside `core`) names the per-mount XPC service the extension
    /// connects to.
    pub fn start(core: MountCore, mount_point: &Path, state_dir: &Path) -> Result<Self> {
        let mount_id = core.mount_id().to_string();
        let sock = socket_path(state_dir);

        let server = Arc::new(server::MountServer::new(Arc::new(core)));
        let ipc = ipc::IpcServer::start(&sock, server)
            .map_err(|e| OakError::Io(std::io::Error::other(format!("fskit ipc: {e}"))))?;

        if let Err(e) = run_mount(mount_point, state_dir, &mount_id, &sock) {
            // Mounting failed — wind the server back down so we don't leak the
            // socket/thread, then surface the (actionable) error.
            drop(ipc);
            return Err(e);
        }

        Ok(Self {
            ipc,
            mount_point: mount_point.to_path_buf(),
        })
    }

    /// Unmount the volume and stop the IPC server.
    pub fn stop(mut self) -> Result<()> {
        let res = unmount(&self.mount_point);
        self.ipc.shutdown();
        res
    }
}

/// Invoke `/sbin/mount` to bring up the synthetic `OakFS` volume.
///
/// FSKit synthetic (non-block-device) volumes are mounted via `FSPathURLResource`
/// and cannot be initiated through DiskArbitration, so we call `/sbin/mount`
/// directly — exactly what `diskarbitrationd` does internally. The daemon is
/// not sandboxed, so this is allowed.
///
/// We pass the mount id and socket path through mount options; the extension
/// reads them in its `loadResource`/`probeResource` to find this daemon. The
/// `<resource>` argument is the mount state dir, which the extension receives
/// as a security-scoped URL.
fn run_mount(mount_point: &Path, state_dir: &Path, mount_id: &str, sock: &Path) -> Result<()> {
    let opts = format!("nobrowse,oak_id={},oak_socket={}", mount_id, sock.display());
    let output = Command::new("/sbin/mount")
        .arg("-F") // synthetic / FSKit-resource mount, not DiskArbitration
        .args(["-t", FS_SHORT_NAME])
        .args(["-o", &opts])
        .arg(state_dir) // resource (security-scoped path URL)
        .arg(mount_point)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running /sbin/mount: {e}"))))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The most common failure is the extension not being installed/enabled,
    // which surfaces as an unknown filesystem type. Give the user the fix.
    if stderr.contains("unknown")
        || stderr.contains("not supported")
        || stderr.contains(FS_SHORT_NAME)
    {
        return Err(OakError::Server(format!(
            "Could not mount: the OakFS file-system extension isn't enabled.\n\
             Install and open \"Oak Mounter\", then enable OakFS in System Settings →\n\
             General → Login Items & Extensions → File System Extensions.\n\
             (mount said: {})",
            stderr.trim()
        )));
    }
    Err(OakError::Server(format!(
        "/sbin/mount failed: {}",
        stderr.trim()
    )))
}

/// Unmount the volume. `umount` works for FSKit mounts owned by the calling
/// user, same as the shared `platform_unmount` in `mod.rs`.
fn unmount(mount_point: &Path) -> Result<()> {
    let output = Command::new("umount")
        .arg(mount_point)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running umount: {e}"))))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not currently mounted") || stderr.contains("Invalid argument") {
        return Ok(());
    }
    let force = Command::new("diskutil")
        .args(["unmount", "force"])
        .arg(mount_point)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running diskutil: {e}"))))?;
    if force.status.success() {
        return Ok(());
    }
    Err(OakError::Server(format!(
        "umount failed: {}",
        stderr.trim()
    )))
}
