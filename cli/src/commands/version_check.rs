//! Once-a-day "a new version is available" check.
//!
//! This runs opportunistically after a successful command (see `main`). It is
//! deliberately best-effort: any failure — no network, server down, malformed
//! state file — is swallowed so it can never break or slow down the command the
//! user actually ran. The network request is only made at most once per
//! [`CHECK_INTERVAL_SECS`]; in between, the cached timestamp short-circuits it.
//!
//! Set `OAK_NO_UPDATE_CHECK` to disable entirely.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::commands::upgrade::{fetch_latest_tag, is_newer_version};
use crate::output::colors;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait between network checks (24 hours).
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Persisted state at `~/.oak/version_check`. `last_check` is a Unix timestamp
/// (seconds) of the last *attempt* — written even when the attempt failed, so a
/// flaky or offline server doesn't trigger a network hit on every command.
#[derive(Serialize, Deserialize, Default)]
struct State {
    last_check: u64,
    /// Latest version the last successful check saw, if any.
    latest_known: Option<String>,
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

/// Query GitHub Releases for the latest published version tag. Short timeout;
/// returns `None` on any error or if no releases are published. Shares the
/// redirect-based lookup with `oak upgrade` (see [`fetch_latest_tag`]) so the
/// daily notifier and the upgrade command always agree on "latest".
async fn fetch_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .timeout(Duration::from_secs(2))
        // Redirects disabled so `fetch_latest_tag` can read the `Location`
        // header off GitHub's `/releases/latest` 302.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    fetch_latest_tag(&client).await.ok().flatten()
}

/// Print the upgrade prompt to stderr (never stdout, so piped command output
/// like `oak hash` stays clean).
fn notify(current: &str, latest: &str) {
    // This prints to stderr, so gate the colors on stderr's decision (the
    // `Display` impl on `Color` gates on stdout's).
    let bold = colors::BOLD.for_stderr();
    let dim = colors::DIM.for_stderr();
    let green = colors::GREEN.for_stderr();
    let yellow = colors::YELLOW.for_stderr();
    let reset = colors::RESET.for_stderr();
    eprintln!();
    eprintln!(
        "{yellow}{bold}A new version of oak is available:{reset} \
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

    // Within the check window: don't touch the network at all.
    if let Some(state) = &state {
        if now.saturating_sub(state.last_check) < CHECK_INTERVAL_SECS {
            return;
        }
    }

    let latest = rt.block_on(fetch_latest());

    // Record the attempt regardless of outcome so we don't re-check until the
    // window elapses again.
    save_state(
        &path,
        &State {
            last_check: now,
            latest_known: latest.clone(),
        },
    );

    if let Some(latest) = latest {
        let current = format!("v{VERSION}");
        if is_newer_version(&current, &latest) {
            notify(&current, &latest);
        }
    }
}
