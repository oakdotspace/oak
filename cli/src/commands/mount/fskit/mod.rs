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

/// The broker LaunchAgent's label (and mach service name).
const BROKER_LABEL: &str = "com.oakvcs.mount";

/// Opt-in for a dev build to repoint the shared broker at itself.
const REPOINT_BROKER_ENV: &str = "OAK_FSKIT_REPOINT_BROKER";

/// Ensure the `com.oakvcs.mount` LaunchAgent exists and points at a usable
/// `oak` binary, then make sure launchd has it loaded. The agent is on-demand:
/// launchd starts `oak mount __fskit-broker` only when the sandboxed extension
/// looks up the `com.oakvcs.mount` mach service.
///
/// The agent is shared by every mount on the machine, so this is careful about
/// replacing it (see [`plan_broker`]): a dev build leaves a working installed
/// broker alone, and a loaded broker is never booted out while OakFS volumes
/// are live, because `bootout` permanently invalidates their XPC connections.
fn ensure_broker_launchagent() -> Result<()> {
    let exe = std::env::current_exe().map_err(OakError::Io)?;
    let home = dirs::home_dir()
        .ok_or_else(|| OakError::Server("cannot locate home directory".to_string()))?;
    let agents = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&agents).map_err(OakError::Io)?;
    let plist_path = agents.join(format!("{BROKER_LABEL}.plist"));

    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{BROKER_LABEL}");

    // One-time migration: machines that ran the pre-rename build still have the
    // old `com.oak.mount` agent loaded and its plist on disk. Boot it out and
    // delete the file so the stale service can't shadow the new one. Best-effort;
    // a no-op once the old agent is gone.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/com.oak.mount")])
        .output();
    let _ = std::fs::remove_file(agents.join("com.oak.mount.plist"));

    let existing_plist = std::fs::read_to_string(&plist_path).ok();
    let plist_program = existing_plist.as_deref().and_then(plist_program);
    let loaded_program = Command::new("launchctl")
        .args(["print", &service])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| launchctl_program(&String::from_utf8_lossy(&out.stdout)));
    let state = BrokerState {
        dev_build: is_dev_build(&exe),
        opt_in: std::env::var_os(REPOINT_BROKER_ENV).is_some_and(|v| !v.is_empty()),
        plist_program_exists: plist_program.as_deref().is_some_and(Path::is_file),
        plist_program,
        loaded_program_exists: loaded_program.as_deref().is_some_and(Path::is_file),
        loaded_program,
        live_mounts: live_oakfs_mounts(),
        current_exe: exe,
    };
    let plan = plan_broker(&state);
    output::vlog(&format!("fskit: broker plan {plan:?} for {state:?}"));

    if plan.kept_existing {
        output::vlog(&format!(
            "fskit: dev build; leaving the shared broker on {} (set {REPOINT_BROKER_ENV}=1 to point it at {})",
            plan.program.display(),
            state.current_exe.display()
        ));
    }

    let plist = broker_plist(&plan.program);
    if existing_plist.as_deref() != Some(plist.as_str()) {
        std::fs::write(&plist_path, &plist).map_err(OakError::Io)?;
    }

    match plan.reload {
        BrokerReload::None => return Ok(()),
        BrokerReload::Deferred => {
            output::warning(&format!(
                "the Oak mount broker still runs {}; it will switch to {} once no \
                 OakFS volumes are mounted (restarting it now would break the {} live mount(s))",
                state
                    .loaded_program
                    .as_deref()
                    .unwrap_or(Path::new("?"))
                    .display(),
                plan.program.display(),
                state.live_mounts
            ));
            return Ok(());
        }
        BrokerReload::Replace => {
            let _ = Command::new("launchctl")
                .args(["bootout", &service])
                .output();
        }
        BrokerReload::Bootstrap => {}
    }
    output::vlog(&format!(
        "fskit: bootstrapping broker launchagent {} into {domain} (reload={:?})",
        plist_path.display(),
        plan.reload
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

/// What [`ensure_broker_launchagent`] found on this machine.
#[derive(Debug)]
struct BrokerState {
    current_exe: PathBuf,
    /// `current_exe` is a local cargo build rather than an installed release.
    dev_build: bool,
    /// [`REPOINT_BROKER_ENV`] is set.
    opt_in: bool,
    /// The program named by the LaunchAgent plist on disk.
    plist_program: Option<PathBuf>,
    plist_program_exists: bool,
    /// The program of the broker launchd currently has loaded, if any.
    loaded_program: Option<PathBuf>,
    loaded_program_exists: bool,
    /// OakFS volumes mounted right now (not counting the one being started).
    live_mounts: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct BrokerPlan {
    /// The program the plist should name.
    program: PathBuf,
    /// A dev build kept the existing broker instead of installing itself.
    kept_existing: bool,
    reload: BrokerReload,
}

#[derive(Debug, PartialEq, Eq)]
enum BrokerReload {
    /// launchd already runs the planned program.
    None,
    /// Nothing is loaded; bootstrap the plist.
    Bootstrap,
    /// Boot out the loaded broker and bootstrap the new plist.
    Replace,
    /// A different broker is loaded but live mounts depend on its connections;
    /// leave it running. The rewritten plist takes over on a later mount.
    Deferred,
}

/// Decide which binary the shared broker should run and how to get launchd
/// there. The broker only forwards bytes between the extension and each
/// mount's socket, so any working `oak` can serve any daemon: a dev build
/// never needs to replace an installed broker, and a working broker never
/// needs to be restarted under live mounts.
fn plan_broker(state: &BrokerState) -> BrokerPlan {
    let keep = state.dev_build
        && !state.opt_in
        && state.plist_program_exists
        && state.plist_program.as_deref() != Some(state.current_exe.as_path());
    let program = match (&state.plist_program, keep) {
        (Some(existing), true) => existing.clone(),
        _ => state.current_exe.clone(),
    };
    let reload = match state.loaded_program.as_deref() {
        None => BrokerReload::Bootstrap,
        Some(loaded) if loaded == program => BrokerReload::None,
        // A broker whose binary is gone can't respawn, so replacing it can't
        // make things worse.
        Some(_) if state.loaded_program_exists && state.live_mounts > 0 => BrokerReload::Deferred,
        Some(_) => BrokerReload::Replace,
    };
    BrokerPlan {
        program,
        kept_existing: keep,
        reload,
    }
}

/// Whether `exe` is a local cargo build: a debug build, or any binary sitting in
/// a cargo target profile directory (which holds `.fingerprint/` and `deps/`).
fn is_dev_build(exe: &Path) -> bool {
    cfg!(debug_assertions)
        || exe
            .parent()
            .is_some_and(|dir| dir.join(".fingerprint").is_dir() && dir.join("deps").is_dir())
}

fn broker_plist(program: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{BROKER_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>mount</string>
        <string>__fskit-broker</string>
    </array>
    <key>MachServices</key>
    <dict><key>{BROKER_LABEL}</key><true/></dict>
</dict>
</plist>
"#,
        program = program.display()
    )
}

/// The first `ProgramArguments` entry of a broker plist we wrote.
fn plist_program(plist: &str) -> Option<PathBuf> {
    let args = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = args.find("<string>")? + "<string>".len();
    let len = args[start..].find("</string>")?;
    Some(PathBuf::from(&args[start..start + len]))
}

/// The `program = …` line of `launchctl print <service>`.
fn launchctl_program(print: &str) -> Option<PathBuf> {
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("program = "))
        .map(PathBuf::from)
}

/// Count mounted OakFS volumes from `/sbin/mount`'s table
/// (`… on /path (OakFS, local, …)`). An unreadable table counts as none, so a
/// broken broker can still be replaced.
fn live_oakfs_mounts() -> usize {
    Command::new("/sbin/mount")
        .output()
        .map(|out| count_oakfs_mounts(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(0)
}

fn count_oakfs_mounts(table: &str) -> usize {
    let marker = format!("({FS_SHORT_NAME},");
    table.lines().filter(|l| l.contains(&marker)).count()
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
// `install.sh` uses (`/api/releases/<version>/darwin-mounter`). `oak upgrade`
// then keeps an installed app on the same release as the CLI.

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

/// The dir `Oak Mount.app` is installed in, if it's in any known location.
#[cfg(target_os = "macos")]
fn installed_mounter_dir() -> Option<PathBuf> {
    mounter_app_dirs()
        .into_iter()
        .find(|d| d.join(APP_BUNDLE).is_dir())
}

/// `~/.oak/mounter-build`: the build the installed app came from, as `oak
/// upgrade` compares it. The bundle can't say: its version is fixed in
/// `project.yml`, not stamped per release. Missing for an app this CLI didn't
/// install (`install.sh`, or a CLI too old to write it), which reads as stale.
#[cfg(target_os = "macos")]
fn mounter_build_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oak").join("mounter-build"))
}

#[cfg(target_os = "macos")]
fn load_mounter_build(path: &Path) -> Option<String> {
    let build = std::fs::read_to_string(path).ok()?;
    Some(build.trim().to_string())
}

#[cfg(target_os = "macos")]
fn save_mounter_build(path: &Path, build: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::atomic_file::write_atomic(path, format!("{build}\n"))
}

/// Launch the Oak Mount app in the background so live mounts show up in its
/// menu-bar extra. `open -g` doesn't steal focus and is a no-op when the app
/// is already running; `--background` tells the app not to open its setup
/// window unless the extension needs attention. Skipped when
/// `OAK_NO_MOUNT_TRAY` is set or the app isn't installed. Best-effort.
#[cfg(target_os = "macos")]
pub fn launch_status_app() {
    open_status_app();
}

/// Run `open` for `launch_status_app`, returning false only if it failed.
#[cfg(target_os = "macos")]
fn open_status_app() -> bool {
    if std::env::var_os("OAK_NO_MOUNT_TRAY").is_some() {
        return true;
    }
    let Some(app) = mounter_app_dirs()
        .into_iter()
        .map(|d| d.join(APP_BUNDLE))
        .find(|p| p.is_dir())
    else {
        return true;
    };
    Command::new("/usr/bin/open")
        .arg("-g")
        .arg(&app)
        .args(["--args", "--background"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Start the app again after `stop_mounter_app`. LaunchServices keeps refusing
/// `open` (error -600) for about a second after the old instance exits, so
/// retry rather than leave the menu-bar extra gone. Retrying instead of
/// `open -n` can't start a second copy. Best-effort: the next mount starts it.
#[cfg(target_os = "macos")]
fn relaunch_status_app() {
    for _ in 0..40 {
        if open_status_app() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
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
    if installed_mounter_dir().is_some() {
        return Ok(());
    }

    let base = crate::commands::upgrade::release_base_url();
    let (version, build) = crate::commands::upgrade::running_release();
    output::info(&format!(
        "First mount on this Mac — installing the Oak Mount app ({version})…"
    ));

    let zip = download_mounter_zip(&base, &version).await?;
    let app_dir = install_mounter_app(&zip)?;
    if let Some(path) = mounter_build_path() {
        let _ = save_mounter_build(&path, &build);
    }

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

/// `oak upgrade`'s half of the app: bring an installed app to `build`, the CLI
/// build it pairs with, published under `release` (`v0.106.0` or `nightly`).
///
/// - Not installed → nothing to do: CLI-only installs stay that way, and the
///   first `oak mount` installs the matching app.
/// - Already on `build` → nothing to do, with no network.
/// - Any mount live → postponed, not replaced: the extension serving it runs
///   out of this bundle. The build isn't recorded, so `oak upgrade` picks it
///   up once the mounts are gone.
///
/// Stays in the dir the app is already in, so an app in `~/Applications` isn't
/// duplicated into `/Applications`. Registering the new extension is enough for
/// mounts, and the user's OakFS toggle belongs to the bundle id, not the copy.
/// A running app is a menu-bar agent that would otherwise stay on the old code,
/// so it's restarted in the background; one that wasn't running stays that way.
#[cfg(target_os = "macos")]
pub async fn upgrade_mounter(release: &str, build: &str) -> Result<()> {
    let Some(dir) = installed_mounter_dir() else {
        return Ok(());
    };
    let build_path = mounter_build_path();
    if build_path
        .as_deref()
        .and_then(load_mounter_build)
        .as_deref()
        == Some(build)
    {
        return Ok(());
    }
    let live = live_oakfs_mounts();
    if live > 0 {
        output::info(&format!(
            "Not updating the Oak Mount app while {live} mount(s) are live. Run `oak mount list`, unmount them, then run `oak upgrade` again."
        ));
        return Ok(());
    }

    output::info(&format!("Updating the Oak Mount app ({build})..."));
    let base = crate::commands::upgrade::release_base_url();
    let zip = download_mounter_zip(&base, release).await?;
    replace_mounter_app(&zip, &dir)?;

    let appex = dir
        .join(APP_BUNDLE)
        .join("Contents/Extensions/OakFSExtension.appex");
    let _ = Command::new("pluginkit").arg("-a").arg(appex).status();
    if let Some(path) = build_path {
        save_mounter_build(&path, build)?;
    }
    if stop_mounter_app(&dir) {
        relaunch_status_app();
    }
    output::success(&format!("Updated \"{APP_BUNDLE}\" in {}.", dir.display()));
    Ok(())
}

/// Stop the app running out of `dir`, returning whether it was. Waits for it to
/// exit: `open` would otherwise find the dying instance and start nothing.
#[cfg(target_os = "macos")]
fn stop_mounter_app(dir: &Path) -> bool {
    stop_process_running_from(&dir.join(APP_BUNDLE).join("Contents/MacOS/Oak Mount"))
}

/// Stop every process whose command line starts with `exe`, returning whether
/// there was one, and wait for it to exit. Anchored and regex-escaped, so only
/// a copy running from exactly that path matches.
#[cfg(target_os = "macos")]
fn stop_process_running_from(exe: &Path) -> bool {
    let pattern = format!("^{}", regex::escape(&exe.to_string_lossy()));
    let signaled = |tool: &str| {
        Command::new(tool)
            .arg("-f")
            .arg(&pattern)
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !signaled("pkill") {
        return false;
    }
    for _ in 0..30 {
        if !signaled("pgrep") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    true
}

/// Fetch the app zip from `/api/releases/<version>/darwin-mounter` and verify
/// its minisign sidecar before installing. SHA-256 is still checked as a
/// corruption guard, but authenticity comes from the release signing key.
#[cfg(target_os = "macos")]
async fn download_mounter_zip(base: &str, version: &str) -> Result<Vec<u8>> {
    use crate::commands::upgrade::{verify_release_signature, RELEASE_PUBKEY};
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

    let sig_resp = client
        .get(format!("{zip_url}/minisig"))
        .header("user-agent", &ua)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !sig_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Oak Mount app {version} has no release signature (HTTP {}) — refusing to install untrusted bytes.",
            sig_resp.status()
        )));
    }
    let signature = sig_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    verify_release_signature(RELEASE_PUBKEY, &signature, &bytes)?;

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

/// Stage app zip bytes in a tempfile: `ditto` reads the archive from a real path.
#[cfg(target_os = "macos")]
fn stage_mounter_zip(zip_bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut tmp = tempfile::Builder::new()
        .prefix("oak-mounter-")
        .suffix(".zip")
        .tempfile()
        .map_err(OakError::Io)?;
    tmp.write_all(zip_bytes).map_err(OakError::Io)?;
    Ok(tmp)
}

/// Swap the app bundle in `dir` for the one in the `ditto`-packaged zip. It
/// unpacks beside the installed bundle and is renamed over it, so any failure
/// leaves the installed app untouched and registered.
#[cfg(target_os = "macos")]
fn replace_mounter_app(zip_bytes: &[u8], dir: &Path) -> Result<()> {
    let tmp = stage_mounter_zip(zip_bytes)?;
    let stage = tempfile::Builder::new()
        .prefix(".oak-mounter-")
        .tempdir_in(dir)
        .map_err(OakError::Io)?;

    let out = Command::new("ditto")
        .arg("-x")
        .arg("-k")
        .arg(tmp.path())
        .arg(stage.path())
        .output()
        .map_err(OakError::Io)?;
    let staged = stage.path().join(APP_BUNDLE);
    if !out.status.success() || !staged.is_dir() {
        return Err(OakError::Server(format!(
            "Couldn't unpack the new \"{APP_BUNDLE}\": {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    // Dropping `stage` cleans up the old bundle along with it.
    let installed = dir.join(APP_BUNDLE);
    let previous = stage.path().join("previous.app");
    std::fs::rename(&installed, &previous)?;
    if let Err(e) = std::fs::rename(&staged, &installed) {
        let _ = std::fs::rename(&previous, &installed);
        return Err(OakError::Io(e));
    }
    Ok(())
}

/// Extract the `ditto`-packaged app zip into the first writable app dir,
/// replacing any prior copy. `ditto -x -k` restores the bundle exactly,
/// preserving the notarized code signature. Returns the dir it landed in.
#[cfg(target_os = "macos")]
fn install_mounter_app(zip_bytes: &[u8]) -> Result<PathBuf> {
    let tmp = stage_mounter_zip(zip_bytes)?;
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

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    /// A `ditto --keepParent` zip of an app bundle holding `marker`, the shape
    /// the release ships.
    fn app_zip(root: &Path, marker: &str) -> Vec<u8> {
        let src = root.join(format!("src-{marker}"));
        let app = src.join(APP_BUNDLE);
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/marker"), marker).unwrap();
        let zip = root.join(format!("{marker}.zip"));
        let status = Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&app)
            .arg(&zip)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(zip).unwrap()
    }

    fn installed_marker(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(APP_BUNDLE).join("Contents/marker")).unwrap()
    }

    fn entries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn replacing_the_app_swaps_the_bundle_and_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Applications");
        std::fs::create_dir_all(&dir).unwrap();
        install_fake_app(&dir, &app_zip(root.path(), "old"));
        std::fs::write(dir.join(APP_BUNDLE).join("Contents/old-only"), "x").unwrap();

        replace_mounter_app(&app_zip(root.path(), "new"), &dir).unwrap();

        assert_eq!(installed_marker(&dir), "new");
        // Replaced, not merged: nothing of the old bundle survives.
        assert!(!dir.join(APP_BUNDLE).join("Contents/old-only").exists());
        assert_eq!(entries(&dir), vec![APP_BUNDLE.to_string()]);
    }

    #[test]
    fn a_bad_zip_leaves_the_installed_app_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Applications");
        std::fs::create_dir_all(&dir).unwrap();
        install_fake_app(&dir, &app_zip(root.path(), "old"));

        let err = replace_mounter_app(b"not a zip", &dir).expect_err("bad zip must fail");

        assert!(err.to_string().contains("Couldn't unpack"), "{err}");
        assert_eq!(installed_marker(&dir), "old");
        assert_eq!(entries(&dir), vec![APP_BUNDLE.to_string()]);
    }

    /// Install `zip` into `dir` the way a real first install lands it.
    fn install_fake_app(dir: &Path, zip: &[u8]) {
        let tmp = stage_mounter_zip(zip).unwrap();
        let status = Command::new("ditto")
            .args(["-x", "-k"])
            .arg(tmp.path())
            .arg(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Never run this out of an `.app` directory: macOS shows the logged-in
    /// user a "damaged app" alert for an executed fake bundle. And never
    /// *copy* a system binary to run it: a copied Apple-signed binary fails
    /// its platform signature check and is killed at once (signal 9), which
    /// races every assertion below. A symlink keeps the real binary (and its
    /// signature) while the process's command line shows the link's path,
    /// which is what `stop_process_running_from` matches.
    #[test]
    fn stopping_ends_only_a_copy_running_from_that_path() {
        let root = tempfile::tempdir().unwrap();
        // A `+` is a regex operator, so this also proves the path is escaped.
        let dir = root.path().join("Apps+1").join("Oak Mount.bundle-test");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("Oak Mount");
        std::os::unix::fs::symlink("/bin/sleep", &exe).unwrap();
        // Same name in a sibling dir: must survive.
        let other_dir = root.path().join("Apps+2");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other_exe = other_dir.join("Oak Mount");
        std::os::unix::fs::symlink("/bin/sleep", &other_exe).unwrap();

        assert!(!stop_process_running_from(&exe));

        let mut other = Command::new(&other_exe).arg("30").spawn().unwrap();
        let mut app = Command::new(&exe).arg("30").spawn().unwrap();
        assert!(stop_process_running_from(&exe));
        assert!(!app.wait().unwrap().success());
        assert!(!stop_process_running_from(&exe));
        assert!(other.try_wait().unwrap().is_none(), "sibling was stopped");
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn mounter_build_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".oak").join("mounter-build");

        assert_eq!(load_mounter_build(&path), None);
        save_mounter_build(&path, "20261103.854a614d").unwrap();
        assert_eq!(
            load_mounter_build(&path).as_deref(),
            Some("20261103.854a614d")
        );
    }

    fn broker_state(current: &str) -> BrokerState {
        BrokerState {
            current_exe: PathBuf::from(current),
            dev_build: false,
            opt_in: false,
            plist_program: None,
            plist_program_exists: false,
            loaded_program: None,
            loaded_program_exists: false,
            live_mounts: 0,
        }
    }

    const INSTALLED: &str = "/Users/u/.local/bin/oak";
    const DEV: &str = "/Users/u/target/debug/oak";

    fn installed_broker_loaded(mut state: BrokerState) -> BrokerState {
        state.plist_program = Some(PathBuf::from(INSTALLED));
        state.plist_program_exists = true;
        state.loaded_program = Some(PathBuf::from(INSTALLED));
        state.loaded_program_exists = true;
        state
    }

    #[test]
    fn dev_build_leaves_installed_broker_alone() {
        let mut state = installed_broker_loaded(broker_state(DEV));
        state.dev_build = true;
        assert_eq!(
            plan_broker(&state),
            BrokerPlan {
                program: PathBuf::from(INSTALLED),
                kept_existing: true,
                reload: BrokerReload::None,
            }
        );
    }

    #[test]
    fn dev_build_opt_in_repoints_broker() {
        let mut state = installed_broker_loaded(broker_state(DEV));
        state.dev_build = true;
        state.opt_in = true;
        assert_eq!(
            plan_broker(&state),
            BrokerPlan {
                program: PathBuf::from(DEV),
                kept_existing: false,
                reload: BrokerReload::Replace,
            }
        );
    }

    #[test]
    fn dev_build_installs_itself_when_no_working_broker_exists() {
        let mut state = broker_state(DEV);
        state.dev_build = true;
        let plan = plan_broker(&state);
        assert_eq!(plan.program, PathBuf::from(DEV));
        assert_eq!(plan.reload, BrokerReload::Bootstrap);

        // A plist naming a binary that no longer exists is not worth keeping.
        state.plist_program = Some(PathBuf::from(INSTALLED));
        let plan = plan_broker(&state);
        assert_eq!(plan.program, PathBuf::from(DEV));
        assert!(!plan.kept_existing);
    }

    #[test]
    fn release_build_repoints_broker_when_no_mounts_are_live() {
        let state = installed_broker_loaded(broker_state("/opt/homebrew/bin/oak"));
        let plan = plan_broker(&state);
        assert_eq!(plan.program, PathBuf::from("/opt/homebrew/bin/oak"));
        assert_eq!(plan.reload, BrokerReload::Replace);
    }

    #[test]
    fn broker_is_not_booted_out_under_live_mounts() {
        let mut state = installed_broker_loaded(broker_state("/opt/homebrew/bin/oak"));
        state.live_mounts = 2;
        assert_eq!(plan_broker(&state).reload, BrokerReload::Deferred);

        // ...unless its binary is gone, so it could never respawn anyway.
        state.loaded_program_exists = false;
        assert_eq!(plan_broker(&state).reload, BrokerReload::Replace);
    }

    #[test]
    fn matching_broker_is_left_loaded() {
        let mut state = installed_broker_loaded(broker_state(INSTALLED));
        state.live_mounts = 3;
        assert_eq!(plan_broker(&state).reload, BrokerReload::None);
    }

    #[test]
    fn cargo_target_dir_binaries_are_dev_builds() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("release");
        std::fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        assert!(is_dev_build(&profile.join("oak")));
        // Test binaries carry debug assertions, so only the positive case is
        // meaningful for a non-cargo path here.
        assert_eq!(
            is_dev_build(&dir.path().join("oak")),
            cfg!(debug_assertions)
        );
    }

    #[test]
    fn broker_plist_round_trips_its_program() {
        let plist = broker_plist(Path::new(INSTALLED));
        assert_eq!(plist_program(&plist), Some(PathBuf::from(INSTALLED)));
        assert!(plist.contains("<key>MachServices</key>"));
    }

    #[test]
    fn launchctl_print_program_is_parsed() {
        let print = "gui/501/com.oakvcs.mount = {\n\tactive count = 1\n\tpath = /Users/u/Library/LaunchAgents/com.oakvcs.mount.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Users/u/.local/bin/oak\n\targuments = {\n";
        assert_eq!(launchctl_program(print), Some(PathBuf::from(INSTALLED)));
        assert_eq!(launchctl_program("state = waiting\n"), None);
    }

    #[test]
    fn oakfs_mounts_are_counted() {
        let table = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
devices -- file:///x/ on /y (devicefs, local, nodev, fskit, mounted by u)\n\
file:///Users/u/.oak/mounts/a/ on /Users/u/oak/a (OakFS, local, nodev, nosuid, fskit, mounted by u)\n\
file:///Users/u/.oak/mounts/b/ on /Users/u/oak/b (OakFS, local, nodev, nosuid, fskit, mounted by u)\n";
        assert_eq!(count_oakfs_mounts(table), 2);
        assert_eq!(count_oakfs_mounts(""), 0);
    }

    #[tokio::test]
    async fn mounter_download_rejects_missing_minisig() {
        let server = MockServer::start().await;
        let version = "v9.9.9";
        Mock::given(method("GET"))
            .and(path(format!("/api/releases/{version}/{MOUNTER_PLATFORM}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake app zip"))
            .mount(&server)
            .await;

        let err = download_mounter_zip(&server.uri(), version)
            .await
            .expect_err("missing minisig must fail closed");

        assert!(
            err.to_string().contains("has no release signature"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn mounter_download_rejects_malformed_minisig() {
        let server = MockServer::start().await;
        let version = "v9.9.9";
        let release_path = format!("/api/releases/{version}/{MOUNTER_PLATFORM}");
        Mock::given(method("GET"))
            .and(path(release_path.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake app zip"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{release_path}/minisig")))
            .respond_with(ResponseTemplate::new(200).set_body_string("not a signature"))
            .mount(&server)
            .await;

        let err = download_mounter_zip(&server.uri(), version)
            .await
            .expect_err("malformed minisig must fail closed");

        assert!(
            err.to_string().contains("Malformed release signature"),
            "unexpected error: {err}"
        );
    }
}
