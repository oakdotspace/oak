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
//! enables the signed `Oak Mount` app once; there is no macFUSE kext, no
//! reduced-security boot, and it survives OS upgrades.
//!
//! ### Transport
//! The sandbox forbids the extension from opening the unix socket directly, so
//! the extension speaks XPC to the launchd-vended `com.oakvcs.mount` mach
//! service and the [`broker`] forwards each framed `Op`/`Reply` to the right
//! mount's [`ipc`] socket. Both hops ship with the sandbox on; [`ipc`] also
//! backs the in-process unit tests directly. Building and signing the extension
//! requires Xcode and the `com.apple.developer.fskit.fsmodule` entitlement,
//! granted to the team's provisioning profile. The remaining pre-ship step is a
//! full on-device run through the signed, sandboxed extension (the broker ping
//! only covers the XPC echo path). See `macos/OakFS/README.md`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use oak_core::{OakError, Result};

use super::core::MountCore;
use crate::output;

#[cfg(target_os = "macos")]
pub mod broker;
pub mod ipc;
pub mod protocol;
pub mod server;

/// The filesystem short-name registered by the FSKit extension's Info.plist
/// (`FSShortName`). `mount -t OakFS …` selects it.
pub const FS_SHORT_NAME: &str = "OakFS";

/// The OakFS FSKit extension's bundle id, as registered with `pluginkit`. Used
/// to verify registration positively (`pluginkit -m -i <id>`) when a mount
/// fails, instead of inferring an extension problem from a generic error.
pub const EXTENSION_BUNDLE_ID: &str = "com.oakvcs.mount.Extension";

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

        // The sandboxed extension can't open our socket; it reaches us through
        // the `com.oakvcs.mount` XPC broker, vended by a launchd agent. Make sure
        // that agent is installed and points at *this* oak binary.
        if let Err(e) = ensure_broker_launchagent() {
            drop(ipc);
            return Err(e);
        }

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

/// Ensure the `com.oakvcs.mount` LaunchAgent exists and points at the current
/// `oak` binary, then make sure launchd has it loaded. The agent is on-demand:
/// launchd starts `oak mount __fskit-broker` only when the sandboxed extension
/// looks up the `com.oakvcs.mount` mach service. Idempotent; rewrites + reloads
/// only when the embedded oak path changed (e.g. after an upgrade).
fn ensure_broker_launchagent() -> Result<()> {
    let exe = std::env::current_exe().map_err(OakError::Io)?;
    let home = dirs::home_dir()
        .ok_or_else(|| OakError::Server("cannot locate home directory".to_string()))?;
    let agents = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&agents).map_err(OakError::Io)?;
    let plist_path = agents.join("com.oakvcs.mount.plist");

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.oakvcs.mount</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>mount</string>
        <string>__fskit-broker</string>
    </array>
    <key>MachServices</key>
    <dict><key>com.oakvcs.mount</key><true/></dict>
</dict>
</plist>
"#,
        exe = exe.display()
    );

    let needs_reload = match std::fs::read_to_string(&plist_path) {
        Ok(existing) => existing != plist,
        Err(_) => true,
    };
    if needs_reload {
        std::fs::write(&plist_path, &plist).map_err(OakError::Io)?;
    }

    // Load (or reload) into the GUI domain. `bootstrap` is a no-op-ish error if
    // already loaded, so on a reload we bootout first; both are best-effort.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");

    // One-time migration: machines that ran the pre-rename build still have the
    // old `com.oak.mount` agent loaded and its plist on disk. Boot it out and
    // delete the file so the stale service can't shadow the new one. Best-effort;
    // a no-op once the old agent is gone.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/com.oak.mount")])
        .output();
    let _ = std::fs::remove_file(agents.join("com.oak.mount.plist"));

    if needs_reload {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/com.oakvcs.mount")])
            .output();
    }
    output::vlog(&format!(
        "fskit: bootstrapping broker launchagent {} into {domain} (needs_reload={needs_reload})",
        plist_path.display()
    ));
    let bootstrap = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist_path)
        .output();
    match &bootstrap {
        Ok(out) => {
            // `bootstrap` errors with "service already loaded" (status 5) when the
            // agent is already up — not fatal, hence best-effort. But a *different*
            // failure here means the extension can never reach this daemon, which
            // surfaces later as a cryptic "/sbin/mount: Unable to invoke task".
            let code = out.status.code();
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::debug!(
                ?code,
                stdout = %String::from_utf8_lossy(&out.stdout).trim(),
                stderr = %stderr.trim(),
                "fskit: launchctl bootstrap result"
            );
            if !out.status.success() {
                output::vlog(&format!(
                    "fskit: launchctl bootstrap exited {code:?}: {}",
                    stderr.trim()
                ));
            }
        }
        Err(e) => {
            tracing::warn!(?e, "fskit: launchctl bootstrap failed to run");
            output::vlog(&format!("fskit: launchctl bootstrap failed to run: {e}"));
        }
    }
    Ok(())
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
    // Running as root is a dead end on macOS: FSKit extensions are registered
    // per-user, so `/sbin/mount` as root can't invoke the user's OakFS task
    // ("Unable to invoke task"), and the broker LaunchAgent can't bootstrap into
    // root's nonexistent `gui/0` domain. Fail early with the actual fix instead
    // of letting both steps fail cryptically.
    let euid = unsafe { libc::geteuid() };
    output::vlog(&format!("fskit: running /sbin/mount as euid={euid}"));
    if euid == 0 {
        return Err(OakError::Server(
            "Don't run `oak mount` with sudo. FSKit mounts run as your user — \
             macOS can't invoke the per-user OakFS extension from root. \
             Re-run without sudo."
                .to_string(),
        ));
    }

    // Only standard flags go through `-o`: mount(8) silently drops unknown
    // `-o key=value` sub-options, so they never reach the FSKit task
    // (`FSTaskOptions.taskOptions` arrives empty). The extension instead derives
    // the mount id and daemon socket from the *resource* path (the state dir we
    // pass below) — id = its basename, socket = `<state_dir>/fskit.sock`, which
    // is exactly `sock`. See macos/OakFS/Extension/OakFSUnaryFileSystem.swift.
    let opts = "nobrowse";

    // Log the exact command so a failure can be reproduced by hand. `oak --verbose`
    // (or OAK_VERBOSE=1) prints this to stderr; OAK_LOG=debug captures it via tracing.
    let cmdline = format!(
        "/sbin/mount -F -t {FS_SHORT_NAME} -o {opts} {} {}",
        state_dir.display(),
        mount_point.display()
    );
    output::vlog(&format!("fskit: invoking {cmdline}"));
    tracing::debug!(%cmdline, %mount_id, sock = %sock.display(), "fskit: running /sbin/mount (extension derives socket from resource path)");

    let output = Command::new("/sbin/mount")
        .arg("-F") // synthetic / FSKit-resource mount, not DiskArbitration
        .args(["-t", FS_SHORT_NAME])
        .args(["-o", opts])
        .arg(state_dir) // resource (security-scoped path URL) — carries id + socket
        .arg(mount_point)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running /sbin/mount: {e}"))))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();
    tracing::debug!(
        success = output.status.success(),
        ?code,
        stdout = %stdout.trim(),
        stderr = %stderr.trim(),
        "fskit: /sbin/mount returned"
    );

    if output.status.success() {
        output::vlog("fskit: /sbin/mount succeeded");
        return Ok(());
    }

    output::vlog(&format!(
        "fskit: /sbin/mount exited {code:?}\n  stdout: {}\n  stderr: {}",
        stdout.trim(),
        stderr.trim()
    ));

    // The mount(2) syscall itself failing with EPERM ("Operation not permitted")
    // is NOT an extension problem: the unified log shows the whole FSKit pipeline
    // (Probe, loadResource, activateVolume, getRootFileHandle) succeed and the
    // appex launch+connect, and only the final `mount(2)` returns errno 1. The
    // usual cause is macOS refusing to mount a filesystem into a TCC-protected
    // folder (~/Documents, ~/Desktop, ~/Downloads, iCloud Drive) unless the
    // responsible terminal has Full Disk Access. Catch this first so we don't
    // send the user down the extension-registration rabbit hole.
    if stderr.contains("Operation not permitted") {
        if let Some(label) = protected_dir_label(mount_point) {
            return Err(OakError::Server(format!(
                "Can't mount into {dest}: macOS blocks mounting a filesystem into \
                 {label} (a privacy-protected folder).\n\
                 Either mount somewhere else (e.g. ~/oaktree/...) or grant your \
                 terminal Full Disk Access in\n\
                 System Settings → Privacy & Security → Full Disk Access, then \
                 fully quit and reopen the terminal.\n\
                 (mount exited {code:?}: {stderr})",
                dest = mount_point.display(),
                stderr = stderr.trim(),
            )));
        }
        // EPERM into a path we don't recognize as protected: still a
        // privacy/permissions block from the kernel, not a missing extension.
        // Full Disk Access (or a different destination) is the fix.
        return Err(OakError::Server(format!(
            "Can't mount at {dest}: macOS refused the mount with \"Operation not \
             permitted\".\n\
             This is a privacy/permissions block, not a problem with the OakFS \
             extension. Try mounting under\n\
             your home directory (e.g. ~/oaktree/...), or grant your terminal Full \
             Disk Access in System Settings →\n\
             Privacy & Security → Full Disk Access, then fully quit and reopen the \
             terminal.\n\
             (mount exited {code:?}: {stderr})",
            dest = mount_point.display(),
            stderr = stderr.trim(),
        )));
    }

    // Not an EPERM mount refusal, so this looks like a genuine FSKit
    // extension-discovery failure. Ask pluginkit positively whether the
    // extension is registered/enabled rather than guessing from stderr alone.
    let state = extension_registration();
    output::vlog(&format!("fskit: pluginkit extension state = {state:?}"));

    // When FSKit *recognizes* the module but it's toggled off, mount prints
    // "Module <id> is disabled!" — the clean, unambiguous "go enable it" case.
    // pluginkit's `-` flag reports the same condition even when mount's wording
    // differs. (If instead you see "No extension with fsShortName" / a bare
    // "Unable to invoke task" with the module NOT recognized, the extension's
    // Info.plist is malformed — every FSKit key must be nested inside
    // EXAppExtensionAttributes; see macos/OakFS/project.yml.)
    if stderr.contains("is disabled") || state == ExtensionState::Disabled {
        return Err(OakError::Server(format!(
            "Could not mount: the OakFS file-system extension is installed but disabled.\n\
             Enable it in System Settings → General → Login Items & Extensions →\n\
             File System Extensions (toggle OakFS on), then retry.\n\
             (mount said: {})",
            stderr.trim()
        )));
    }
    // Other extension-discovery failures: not installed, not registered, or a
    // malformed Info.plist. Either pluginkit positively says it isn't registered,
    // or mount emitted a discovery-style error. Point at the registration fix.
    if state == ExtensionState::NotRegistered
        || stderr.contains("unknown")
        || stderr.contains("not supported")
        || stderr.contains("Unable to invoke task")
        || stderr.contains(FS_SHORT_NAME)
    {
        return Err(OakError::Server(format!(
            "Could not mount: the OakFS file-system extension isn't available.\n\
             Open \"Oak Mount\" once to register it, then enable OakFS in System\n\
             Settings → General → Login Items & Extensions → File System Extensions.\n\
             Verify it's registered with: pluginkit -m -i {EXTENSION_BUNDLE_ID}\n\
             (mount exited {code:?}: {})",
            stderr.trim()
        )));
    }
    Err(OakError::Server(format!(
        "/sbin/mount failed (exit {code:?}): {}",
        stderr.trim()
    )))
}

/// macOS TCC-protected directories: the OS refuses to mount a filesystem into
/// one of these (errno EPERM from `mount(2)`) unless the responsible process —
/// the terminal — has Full Disk Access. Returns a short human label for the
/// matched location, or `None` if `dest` is not under a known protected dir.
fn protected_dir_label(dest: &Path) -> Option<&'static str> {
    let home = dirs::home_dir()?;
    let candidates: [(PathBuf, &'static str); 4] = [
        (home.join("Documents"), "~/Documents"),
        (home.join("Desktop"), "~/Desktop"),
        (home.join("Downloads"), "~/Downloads"),
        // ~/Library/Mobile Documents is the on-disk root of iCloud Drive.
        (
            home.join("Library").join("Mobile Documents"),
            "iCloud Drive",
        ),
    ];
    let abs = resolve_existing_abs(dest);
    candidates
        .iter()
        .find(|(root, _)| abs.starts_with(root))
        .map(|(_, label)| *label)
}

/// Best-effort absolute, symlink-resolved form of `dest`. The mount point itself
/// may not exist yet, so fall back to canonicalizing its nearest existing
/// ancestor and re-joining the remainder (this also resolves `~/Documents` if it
/// is a symlink). Returns `dest` unchanged if nothing can be resolved.
fn resolve_existing_abs(dest: &Path) -> PathBuf {
    if let Ok(c) = dest.canonicalize() {
        return c;
    }
    let mut tail = PathBuf::new();
    let mut cur = dest;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail = Path::new(name).join(&tail);
        }
        if let Ok(c) = parent.canonicalize() {
            return c.join(tail);
        }
        cur = parent;
    }
    dest.to_path_buf()
}

/// Registration state of the OakFS FSKit extension, as reported by `pluginkit`.
#[derive(Debug, PartialEq, Eq)]
enum ExtensionState {
    /// Registered with macOS (pluginkit lists it, flag is not `-`).
    Registered,
    /// Registered but disabled by the user (pluginkit flag `-`).
    Disabled,
    /// pluginkit knows of no plugin with our bundle id.
    NotRegistered,
    /// pluginkit couldn't be run or produced output we don't recognize.
    Unknown,
}

/// Query `pluginkit` for the OakFS extension so a registration problem can be
/// reported positively instead of guessed from a generic mount failure.
/// `pluginkit -m -i <id>` prints one line per matching plugin, prefixed by a
/// state flag (`+`/`=` enabled, `-` disabled); no match (and a nonzero exit)
/// means it isn't registered.
fn extension_registration() -> ExtensionState {
    let out = match Command::new("pluginkit")
        .args(["-m", "-i", EXTENSION_BUNDLE_ID])
        .output()
    {
        Ok(o) => o,
        Err(_) => return ExtensionState::Unknown,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.lines().find(|l| l.contains(EXTENSION_BUNDLE_ID)) {
        // The flag is the first non-space character of the listing line.
        Some(line) => match line.trim_start().chars().next() {
            Some('-') => ExtensionState::Disabled,
            Some(_) => ExtensionState::Registered,
            None => ExtensionState::Unknown,
        },
        None => ExtensionState::NotRegistered,
    }
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

// ---------------------------------------------------------------------------
// First-run install of the "Oak Mount" app (macOS only)
// ---------------------------------------------------------------------------
//
// macOS mounts via FSKit, which runs the OakFS filesystem inside a signed host
// app's extension — there's no kernel extension to brew install, but the app
// itself must be present and the extension enabled once. Rather than dead-end a
// brand-new Mac with "Open Oak Mount once to register it" (an app that isn't
// there), `oak mount` installs it on first use from the same release channel
// `install.sh` uses (`/api/releases/<version>/darwin-mounter`).

/// Display name of the host app bundle — matches `make macos-app INSTALL=1` and
/// the bundle inside the `OakMount.zip` the release workflow ships.
#[cfg(target_os = "macos")]
const APP_BUNDLE: &str = "Oak Mount.app";

/// Release "platform" key the app zip is published under, alongside the CLI's
/// `darwin-arm64` etc.
#[cfg(target_os = "macos")]
const MOUNTER_PLATFORM: &str = "darwin-mounter";

/// Candidate install locations, preference order: `/Applications` (expected
/// home), then `~/Applications` (no-sudo fallback).
#[cfg(target_os = "macos")]
fn mounter_app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Applications"));
    }
    dirs
}

/// True if `Oak Mount.app` is already installed in any known location.
#[cfg(target_os = "macos")]
fn mounter_app_installed() -> bool {
    mounter_app_dirs()
        .iter()
        .any(|d| d.join(APP_BUNDLE).is_dir())
}

/// Foreground preflight, run by `oak mount` before it spawns the mount daemon:
/// guarantee the signed Oak Mount app is installed.
///
/// - Already installed → `Ok(())`; the caller proceeds to mount. (Whether the
///   OakFS extension is *enabled* is handled downstream: the daemon's mount
///   attempt fails with the actionable "enable it in System Settings" message,
///   which `spawn_detached` surfaces from the daemon log.)
/// - Not installed → download + checksum-verify + install it, launch it once so
///   macOS registers the extension, then return an actionable error. The mount
///   can't come up until the user toggles OakFS on, so we stop here with
///   instructions instead of spawning a daemon that would just time out.
///
/// Network/disk work only happens on the (rare) not-installed path; the common
/// case is a single `is_dir()` check.
#[cfg(target_os = "macos")]
pub async fn ensure_mounter_ready() -> Result<()> {
    if mounter_app_installed() {
        return Ok(());
    }

    let base = release_base_url();
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    output::info(&format!(
        "First mount on this Mac — installing the Oak Mount app ({version})…"
    ));

    let zip = download_mounter_zip(&base, &version).await?;
    let app_dir = install_mounter_app(&zip)?;

    // Launch once so macOS/pluginkit registers the bundled OakFS extension.
    let _ = Command::new("open").arg(app_dir.join(APP_BUNDLE)).status();

    Err(OakError::Server(format!(
        "Installed \"{APP_BUNDLE}\" to {dir}.\n\
         One more one-time step: enable the OakFS file-system extension in\n\
         System Settings → General → Login Items & Extensions → File System Extensions\n\
         (toggle OakFS on), then re-run your `oak mount` command.\n\
         Verify it's registered with: pluginkit -m -i {ext}",
        dir = app_dir.display(),
        ext = EXTENSION_BUNDLE_ID,
    )))
}

/// Where to fetch releases from. The app zip rides the same channel the
/// `curl … | sh` installer uses; default to the public host, overridable for
/// self-hosted/test servers via `OAK_URL` (the same knob `install.sh` honors).
#[cfg(target_os = "macos")]
fn release_base_url() -> String {
    std::env::var("OAK_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://oak.space".to_string())
}

/// Fetch the app zip from `/api/releases/<version>/darwin-mounter` and verify it
/// against the server-published SHA-256. The app zip carries no minisig (only
/// the CLI binaries are minisign-signed), so SHA-256 over HTTPS is the integrity
/// check — the same one `install.sh` performs.
#[cfg(target_os = "macos")]
async fn download_mounter_zip(base: &str, version: &str) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};

    let client = crate::http::api_client();
    let ua = format!("oak-cli/{}", env!("CARGO_PKG_VERSION"));
    let zip_url = format!("{base}/api/releases/{version}/{MOUNTER_PLATFORM}");

    let resp = client
        .get(&zip_url)
        .header("user-agent", &ua)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(OakError::Server(format!(
            "No Oak Mount app published for {version} at {base} (HTTP {}).\n\
             Install it from {base}, or build it locally with `make macos-app INSTALL=1`.",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
        .to_vec();

    // Pull the published checksum and compare. Refuse to install if we can't
    // get a well-formed checksum — better to fail closed than install unverified
    // bytes into /Applications.
    let sha_resp = client
        .get(format!("{zip_url}/sha256"))
        .header("user-agent", &ua)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !sha_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Couldn't fetch the Oak Mount app checksum for {version} (HTTP {}) — refusing to install unverified.",
            sha_resp.status()
        )));
    }
    let expected = sha_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
        .trim()
        .to_lowercase();

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    if expected.len() != 64 || expected != actual {
        return Err(OakError::Server(format!(
            "Oak Mount app checksum mismatch — refusing to install.\n  expected: {expected}\n  actual:   {actual}"
        )));
    }
    Ok(bytes)
}

/// Extract the `ditto`-packaged app zip into the first writable app dir,
/// replacing any prior copy. `ditto -x -k` restores the bundle exactly,
/// preserving the notarized code signature. Returns the dir it landed in.
#[cfg(target_os = "macos")]
fn install_mounter_app(zip_bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;

    // ditto reads the archive from a real path, so stage the bytes in a tempfile.
    let mut tmp = tempfile::Builder::new()
        .prefix("oak-mounter-")
        .suffix(".zip")
        .tempfile()
        .map_err(OakError::Io)?;
    tmp.write_all(zip_bytes).map_err(OakError::Io)?;
    let zip_path = tmp.path().to_path_buf();

    let mut last_err = String::new();
    for dir in mounter_app_dirs() {
        // /Applications already exists; ~/Applications may need creating.
        let _ = std::fs::create_dir_all(&dir);
        // Remove any stale bundle so ditto restores cleanly rather than merging.
        let _ = std::fs::remove_dir_all(dir.join(APP_BUNDLE));

        let out = Command::new("ditto")
            .arg("-x")
            .arg("-k")
            .arg(&zip_path)
            .arg(&dir)
            .output()
            .map_err(OakError::Io)?;
        if out.status.success() && dir.join(APP_BUNDLE).is_dir() {
            output::success(&format!("Installed \"{APP_BUNDLE}\" to {}.", dir.display()));
            return Ok(dir);
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Err(OakError::Server(format!(
        "Couldn't install \"{APP_BUNDLE}\" into /Applications or ~/Applications: {last_err}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_dirs_are_recognized() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(
            protected_dir_label(&home.join("Documents").join("oak")),
            Some("~/Documents")
        );
        assert_eq!(
            protected_dir_label(&home.join("Desktop").join("x")),
            Some("~/Desktop")
        );
        assert_eq!(
            protected_dir_label(&home.join("Downloads")),
            Some("~/Downloads")
        );
        assert_eq!(
            protected_dir_label(&home.join("Library").join("Mobile Documents").join("repo")),
            Some("iCloud Drive")
        );
    }

    #[test]
    fn unprotected_dirs_are_not_flagged() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        // The canonical recommended location and a non-protected ~/Library path.
        assert_eq!(protected_dir_label(&home.join("oaktree").join("a/b")), None);
        assert_eq!(
            protected_dir_label(&home.join("Library").join("Caches")),
            None
        );
        assert_eq!(protected_dir_label(Path::new("/tmp/oakmnt")), None);
    }
}
