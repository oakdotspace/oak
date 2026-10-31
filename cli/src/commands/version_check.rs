//! Once-a-day "a new version is available" check.
//!
//! This runs opportunistically after a successful command (see `main`). It is
//! deliberately best-effort: any failure — no network, server down, malformed
//! state file — is swallowed so it can never break or slow down the command the
//! user actually ran. The network request is only made at most once per
//! [`CHECK_INTERVAL_SECS`]; in between, the cached timestamp short-circuits it.
//!
//! It follows the update channel `oak upgrade --channel` saved (see
//! [`update_channel`]): stable compares release tags, nightly compares the
//! build id in the nightly's signature with this binary's. Switching channel makes the
//! next command check again instead of waiting out the window.
//!
//! On the nightly channel a new build isn't just announced: it is installed
//! in the background by a detached `oak upgrade --auto` (see
//! [`auto_update_blocked_by`] for when it isn't), which logs to
//! `~/.oak/auto-update.log`. The stable channel only prints the notice.
//!
//! Set `OAK_NO_UPDATE_CHECK` to disable entirely, or `OAK_NO_AUTO_UPDATE` to
//! keep the nightly notice but install by hand. `oak environment --json`
//! reports the effective state.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::commands::update_channel::{self, Channel};
use crate::commands::upgrade::{detect_platform, fetch_latest_version, is_newer_version};
use crate::output::colors;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait between network checks (24 hours).
pub(crate) const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Network timeout for the check (it runs synchronously after the command).
pub(crate) const TIMEOUT_SECS: u64 = 2;

/// Persisted state at `~/.oak/version_check`. `last_check` is a Unix timestamp
/// (seconds) of the last *attempt* — written even when the attempt failed, so a
/// flaky or offline server doesn't trigger a network hit on every command.
#[derive(Serialize, Deserialize, Default)]
struct State {
    last_check: u64,
    /// Latest version (stable) or build id (nightly) the last successful
    /// check saw, if any.
    latest_known: Option<String>,
    /// Channel the last check ran for. Absent in files written before
    /// channels existed, which only ever checked stable.
    #[serde(default)]
    channel: Option<Channel>,
}

fn state_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oak").join("version_check"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_state(path: &PathBuf) -> Option<State> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn save_state(path: &PathBuf, state: &State) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(path, json);
    }
}

/// Ask oak.space for the latest stable release. Short timeout; returns `None`
/// on any error or if nothing is released. Shares the lookup with
/// `oak upgrade` (see [`fetch_latest_version`]) so the daily notifier and the
/// upgrade command always agree on "latest".
async fn fetch_latest() -> Option<String> {
    crate::http::ensure_crypto_provider();
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .ok()?;
    fetch_latest_version(&client).await.ok().flatten()
}

/// Read the current nightly for this platform from its signature. Short
/// timeout; `None` on any error.
async fn fetch_nightly() -> Option<update_channel::NightlyManifest> {
    let platform = detect_platform().ok()?;
    crate::http::ensure_crypto_provider();
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .ok()?;
    let signature = update_channel::fetch_nightly_signature(&client, platform)
        .await
        .ok()?;
    update_channel::NightlyManifest::from_signature(&signature).ok()
}

/// How long an `oak upgrade --auto` lock is honored before it is presumed
/// left behind by a crashed run.
const AUTO_UPDATE_LOCK_STALE_SECS: u64 = 60 * 60;

/// Why this binary won't install nightly builds by itself, or `None` when it
/// will. Development builds never replace themselves: a debug build, or any
/// binary running out of a cargo `target/` directory, is somebody's checkout.
pub(crate) fn auto_update_blocked_by() -> Option<&'static str> {
    if std::env::var_os("OAK_NO_AUTO_UPDATE").is_some() {
        return Some("OAK_NO_AUTO_UPDATE");
    }
    if cfg!(debug_assertions) {
        return Some("debug_build");
    }
    match std::env::current_exe() {
        Ok(exe) if is_cargo_output(&exe) => Some("cargo_target_dir"),
        Ok(_) => None,
        Err(_) => Some("unknown_executable"),
    }
}

/// `target/<profile>/oak` or `target/<triple>/<profile>/oak`.
fn is_cargo_output(exe: &Path) -> bool {
    exe.ancestors()
        .skip(2)
        .take(2)
        .any(|dir| dir.file_name() == Some("target".as_ref()))
}

/// Where the background `oak upgrade --auto` writes its output.
pub(crate) fn auto_update_log_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oak").join("auto-update.log"))
}

/// Held by a running `oak upgrade --auto` so two commands finishing at once
/// don't both swap the binary. Removed on drop.
pub(crate) struct AutoUpdateLock(PathBuf);

impl AutoUpdateLock {
    /// `None` when another automatic update holds a fresh lock.
    pub(crate) fn acquire() -> Option<Self> {
        let path = dirs::home_dir()?.join(".oak").join("auto-update.lock");
        let create = || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
        };
        if create().is_err() {
            let stale = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= AUTO_UPDATE_LOCK_STALE_SECS);
            if !stale {
                return None;
            }
            let _ = std::fs::remove_file(&path);
            create().ok()?;
        }
        Some(Self(path))
    }
}

impl Drop for AutoUpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Start `oak upgrade --auto` detached from this process, with its output in
/// [`auto_update_log_path`]. Returns whether it started.
fn spawn_auto_update() -> bool {
    let (Ok(exe), Some(log_path)) = (std::env::current_exe(), auto_update_log_path()) else {
        return false;
    };
    let Ok(log) = std::fs::File::create(&log_path) else {
        return false;
    };
    let Ok(log_err) = log.try_clone() else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.args(["upgrade", "--auto"])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);
    cmd.spawn().is_ok()
}

/// Put the updater in its own session so it outlives this process and a
/// Ctrl-C in the terminal that ran the command doesn't reach it.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and the only thing we run in the
    // forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

/// Tell the user a nightly build is being installed in the background.
fn notify_auto_update(current: &str, latest: &str) {
    let bold = colors::BOLD.for_stderr();
    let dim = colors::DIM.for_stderr();
    let green = colors::GREEN.for_stderr();
    let yellow = colors::YELLOW.for_stderr();
    let reset = colors::RESET.for_stderr();
    eprintln!();
    eprintln!(
        "{yellow}{bold}Updating oak to the latest nightly build in the background:{reset} \
         {dim}{current}{reset} → {green}{bold}{latest}{reset}"
    );
    if let Some(log) = auto_update_log_path() {
        eprintln!("{dim}Log: {}{reset}", log.display());
    }
}

/// Print the upgrade prompt to stderr (never stdout, so piped command output
/// like `oak hash` stays clean).
fn notify(headline: &str, current: &str, latest: &str) {
    // This prints to stderr, so gate the colors on stderr's decision (the
    // `Display` impl on `Color` gates on stdout's).
    let bold = colors::BOLD.for_stderr();
    let dim = colors::DIM.for_stderr();
    let green = colors::GREEN.for_stderr();
    let yellow = colors::YELLOW.for_stderr();
    let reset = colors::RESET.for_stderr();
    eprintln!();
    eprintln!(
        "{yellow}{bold}{headline}:{reset} \
         {dim}{current}{reset} → {green}{bold}{latest}{reset}"
    );
    eprintln!("Run {bold}oak upgrade{reset} to update.");
}

/// Check for a newer version at most once per day and, if one exists, prompt
/// the user to run `oak upgrade`. Best-effort: silent on any failure.
pub fn maybe_notify(rt: &Runtime) {
    if std::env::var_os("OAK_NO_UPDATE_CHECK").is_some() {
        return;
    }

    let Some(path) = state_path() else {
        return;
    };

    let now = now_secs();
    let state = load_state(&path);
    let channel = update_channel::selected();

    // Within the check window for the same channel: don't touch the network.
    if let Some(state) = &state {
        if state.channel.unwrap_or_default() == channel
            && now.saturating_sub(state.last_check) < CHECK_INTERVAL_SECS
        {
            return;
        }
    }

    let current = format!("v{VERSION}");
    let (latest_known, notice) = match channel {
        Channel::Stable => {
            let latest = rt.block_on(fetch_latest());
            let notice = latest
                .as_ref()
                .filter(|latest| is_newer_version(&current, latest))
                .map(|latest| {
                    (
                        "A new version of oak is available",
                        update_channel::describe_current(),
                        latest.clone(),
                    )
                });
            (latest, notice)
        }
        Channel::Nightly => {
            let manifest = rt.block_on(fetch_nightly());
            let notice = manifest
                .as_ref()
                .filter(|manifest| {
                    update_channel::nightly_is_update(
                        &current,
                        update_channel::running_nightly_build(),
                        manifest,
                    )
                })
                .map(|manifest| {
                    (
                        "A new nightly build of oak is available",
                        update_channel::describe_current(),
                        manifest.describe(),
                    )
                });
            (manifest.map(|manifest| manifest.build), notice)
        }
    };

    // Record the attempt regardless of outcome so we don't re-check until the
    // window elapses again.
    save_state(
        &path,
        &State {
            last_check: now,
            latest_known,
            channel: Some(channel),
        },
    );

    let Some((headline, current, latest)) = notice else {
        return;
    };
    if channel == Channel::Nightly && auto_update_blocked_by().is_none() && spawn_auto_update() {
        notify_auto_update(&current, &latest);
    } else {
        notify(headline, &current, &latest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_outputs_are_dev_builds() {
        assert!(is_cargo_output(Path::new("/src/oak/target/release/oak")));
        assert!(is_cargo_output(Path::new(
            "/src/oak/target/aarch64-apple-darwin/release/oak"
        )));
        assert!(!is_cargo_output(Path::new("/Users/me/.local/bin/oak")));
        assert!(!is_cargo_output(Path::new("/usr/local/bin/oak")));
        // Only the directories cargo itself lays out count.
        assert!(!is_cargo_output(Path::new("/target/a/b/c/oak")));
    }
}
