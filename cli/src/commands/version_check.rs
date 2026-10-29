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
//! Set `OAK_NO_UPDATE_CHECK` to disable entirely. `oak environment --json`
//! reports the effective state.

use std::path::PathBuf;
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

    if let Some((headline, current, latest)) = notice {
        notify(headline, &current, &latest);
    }
}
