//! Which release channel `oak upgrade` and the daily update notice follow,
//! and which channel this binary itself was built for.
//!
//! Both are served by oak.space's release API (`OAK_URL`):
//!
//! - **stable** (the default): tagged `vX.Y.Z` releases, the newest of which
//!   is `/api/releases/latest`.
//! - **nightly**: a build of the tip of `main` that Oak CI's
//!   `.oak/workflows/nightly.yml` publishes once a day (skipping days when
//!   nothing landed). oak.space serves it under the fixed release version
//!   [`NIGHTLY_VERSION`], which is never promoted, so it never becomes
//!   `/api/releases/latest`. Every nightly artifact is minisigned with a
//!   trusted comment naming its version and build (see [`NightlyManifest`]),
//!   so a check costs one small signature download instead of a whole binary.
//!
//! The choice lives in `~/.oak/update.json`, written by
//! `oak upgrade --channel stable|nightly`. A missing or unreadable file means
//! stable. The one-off `oak upgrade --canary` (install the current nightly
//! once) is not a channel and never touches the file.
//!
//! Release builds learn their own channel at compile time from
//! `OAK_BUILD_CHANNEL` and `OAK_BUILD_ID` (the nightly workflow sets both; an
//! unset or empty value means an ordinary stable/dev build). The version
//! itself stays `CARGO_PKG_VERSION`: servers key off it, so a nightly reports
//! the version it was built from.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use minisign_verify::Signature;
use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use crate::commands::upgrade::{is_newer_version, release_base_url, release_url};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release version the rolling nightly occupies on oak.space (see the module
/// docs). The nightly workflow re-stages it each night.
pub(crate) const NIGHTLY_VERSION: &str = "nightly";

/// First word of a nightly artifact's trusted comment; the rest are
/// `key=value` pairs (see [`NightlyManifest::from_signature`]).
const NIGHTLY_COMMENT_TAG: &str = "oak-nightly";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Tagged releases (the default)
    #[default]
    Stable,
    /// A build of the tip of main, published once a day
    Nightly,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
        }
    }
}

/// `~/.oak/update.json`. Parsed leniently: unknown fields are ignored and a
/// channel this binary doesn't know reads as the default.
#[derive(Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    channel: Channel,
}

pub(crate) fn settings_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oak").join("update.json"))
}

fn load_from(path: &Path) -> Channel {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .map(|settings| settings.channel)
        .unwrap_or_default()
}

fn save_to(path: &Path, channel: Channel) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&Settings { channel })
        .map_err(|e| OakError::Server(e.to_string()))?;
    crate::atomic_file::write_atomic(path, format!("{json}\n"))
}

/// The channel the user selected, or stable when they never chose one.
pub fn selected() -> Channel {
    settings_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

/// Remember `channel` for later `oak upgrade` runs and the daily notice.
pub fn select(channel: Channel) -> Result<()> {
    let path = settings_path().ok_or_else(|| {
        OakError::Server("Cannot save the update channel: no home directory".to_string())
    })?;
    save_to(&path, channel)
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.filter(|v| !v.is_empty())
}

/// The channel this binary was built for, when a release build stamped one.
pub fn build_channel() -> Option<&'static str> {
    non_empty(option_env!("OAK_BUILD_CHANNEL"))
}

/// The build's identity within its channel (nightly:
/// `YYYYMMDD.<first 8 hex of the Oak commit>`).
pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("OAK_BUILD_ID"))
}

/// The nightly build id of the running binary; `None` for any other build.
pub fn running_nightly_build() -> Option<&'static str> {
    (build_channel() == Some(Channel::Nightly.as_str()))
        .then(build_id)
        .flatten()
}

/// `0.106.0` plus the build's channel and id when it has them:
/// `0.106.0 (nightly 20261027.b8d19ad7)`. What `oak --version` prints.
pub fn version_line() -> &'static str {
    static LINE: LazyLock<String> =
        LazyLock::new(|| describe(VERSION, build_channel(), build_id()));
    &LINE
}

/// Render a version for humans, with the build's channel and id when known.
pub fn describe(version: &str, channel: Option<&str>, id: Option<&str>) -> String {
    match (channel, id) {
        (Some(channel), Some(id)) => format!("{version} ({channel} {id})"),
        (Some(channel), None) => format!("{version} ({channel})"),
        (None, _) => version.to_string(),
    }
}

/// The running binary, described for humans (`v0.106.0 (nightly …)`).
pub fn describe_current() -> String {
    describe(&format!("v{VERSION}"), build_channel(), build_id())
}

/// The minisign signature of the current nightly binary for `platform`
/// (`darwin-arm64`, …).
pub(crate) fn nightly_signature_url(platform: &str) -> String {
    format!("{}/minisig", release_url(NIGHTLY_VERSION, platform))
}

/// What the current nightly for one platform is, read from the trusted
/// comment of its minisign signature. The nightly workflow signs with
/// `oak-nightly version=v0.106.0 build=20261027.b8d19ad7 commit=<Oak commit>`.
///
/// Until the binary is downloaded the comment is only a hint: verifying the
/// binary against the same signature also authenticates the comment.
#[derive(Debug, PartialEq, Eq)]
pub struct NightlyManifest {
    /// `vX.Y.Z` from the workspace `Cargo.toml` at the built commit.
    pub version: String,
    /// `YYYYMMDD.<short commit>`; also baked into the binary as `OAK_BUILD_ID`.
    pub build: String,
}

impl NightlyManifest {
    /// Parse a `.minisig` file's trusted comment. Unknown keys are ignored.
    pub fn from_signature(signature: &str) -> Result<Self> {
        let malformed = |why: &str| OakError::Server(format!("Malformed nightly signature: {why}"));
        let signature = Signature::decode(signature).map_err(|e| malformed(&e.to_string()))?;
        let mut words = signature.trusted_comment().split_whitespace();
        if words.next() != Some(NIGHTLY_COMMENT_TAG) {
            return Err(malformed("it isn't a nightly build's signature"));
        }
        let (mut version, mut build) = (None, None);
        for (key, value) in words.filter_map(|word| word.split_once('=')) {
            match key {
                "version" => version = Some(value.to_string()),
                "build" => build = Some(value.to_string()),
                _ => {}
            }
        }
        match (version, build) {
            (Some(version), Some(build)) => Ok(Self { version, build }),
            _ => Err(malformed("its trusted comment names no version or build")),
        }
    }

    pub fn describe(&self) -> String {
        describe(
            &self.version,
            Some(Channel::Nightly.as_str()),
            Some(&self.build),
        )
    }
}

/// Fetch the current nightly's signature for `platform`: the up-to-date
/// check reads it ([`NightlyManifest::from_signature`]) and `oak upgrade`
/// verifies the binary against it, so what gets installed is the build that
/// was checked.
pub async fn fetch_nightly_signature(client: &reqwest::Client, platform: &str) -> Result<String> {
    let resp = client
        .get(nightly_signature_url(platform))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(OakError::Server(format!(
            "No nightly build is published for {platform} at {}. Try again tomorrow, or run \
             `oak upgrade --channel stable`.",
            release_base_url()
        )));
    }
    if !status.is_success() {
        return Err(OakError::Server(format!(
            "Unexpected response fetching the nightly signature: HTTP {status}"
        )));
    }
    resp.text().await.map_err(|e| OakError::Http(e.to_string()))
}

/// Whether `manifest` is an update for a binary at `current_version` whose
/// nightly build id is `current_build` (`None` for a non-nightly build).
///
/// Any different build is an update, except that a binary already on a newer
/// version (a stable release that shipped after tonight's build, say) is never
/// moved backwards.
pub fn nightly_is_update(
    current_version: &str,
    current_build: Option<&str>,
    manifest: &NightlyManifest,
) -> bool {
    if is_newer_version(&manifest.version, current_version) {
        return false;
    }
    current_build != Some(manifest.build.as_str())
}

/// Whether `oak upgrade` on the stable channel should install `latest`.
/// Usually that means `latest` is newer; a nightly build also moves to the
/// stable release of its own version, which is how switching back to stable
/// takes effect without waiting for the next release.
pub fn stable_is_update(current_version: &str, running_nightly: bool, latest: &str) -> bool {
    is_newer_version(current_version, latest)
        || (running_nightly && !is_newer_version(latest, current_version))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway minisign key (not the release key) and its signature over
    // b"hello-oak-nightly\n", made with
    // `minisign -S -t "oak-nightly version=v0.106.0 build=20261027.b8d19ad7 commit=b8d19ad7c0ffee00"`.
    const TEST_PUBKEY: &str = "RWQepcBuG51B7akBMSs/ZCLqIF8xYyVg18WnvW7HAqJhNq78XMrdIutP";
    const TEST_PAYLOAD: &[u8] = b"hello-oak-nightly\n";
    const TEST_SIG: &str = "untrusted comment: signature from minisign secret key\n\
RUQepcBuG51B7QkVhlGRTRs9qJxO3rEsl9MVNDJazx4egPDLesOHvFqtcsqg/nlA+G346rLyH0zFE6HhLIXY+pTAy+e33dYZ/QU=\n\
trusted comment: oak-nightly version=v0.106.0 build=20261027.b8d19ad7 commit=b8d19ad7c0ffee00\n\
01d+Gr1olrzrvl7YNkypISw2wAqqNDk0wyXl+oTRuUZ4mkFrBnGIlQI/b+AiqNoPJldxVTu2NwCMuZeO4rhXDA==\n";

    fn manifest(version: &str, build: &str) -> NightlyManifest {
        NightlyManifest {
            version: version.to_string(),
            build: build.to_string(),
        }
    }

    fn with_comment(comment: &str) -> String {
        TEST_SIG.replace(
            "oak-nightly version=v0.106.0 build=20261027.b8d19ad7 commit=b8d19ad7c0ffee00",
            comment,
        )
    }

    #[test]
    fn missing_or_unreadable_settings_mean_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.json");
        assert_eq!(load_from(&path), Channel::Stable);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_from(&path), Channel::Stable);
        // A channel from some future binary reads as the default.
        std::fs::write(&path, r#"{"channel":"weekly"}"#).unwrap();
        assert_eq!(load_from(&path), Channel::Stable);
    }

    #[test]
    fn selection_round_trips_and_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".oak").join("update.json");
        save_to(&path, Channel::Nightly).unwrap();
        assert_eq!(load_from(&path), Channel::Nightly);
        save_to(&path, Channel::Stable).unwrap();
        assert_eq!(load_from(&path), Channel::Stable);
    }

    #[test]
    fn settings_ignore_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.json");
        std::fs::write(&path, r#"{"channel":"nightly","later":true}"#).unwrap();
        assert_eq!(load_from(&path), Channel::Nightly);
    }

    #[test]
    fn describes_builds() {
        assert_eq!(describe("0.106.0", None, None), "0.106.0");
        assert_eq!(
            describe("v0.106.0", Some("nightly"), Some("20261027.b8d19ad7")),
            "v0.106.0 (nightly 20261027.b8d19ad7)"
        );
        assert_eq!(
            describe("0.106.0", Some("nightly"), None),
            "0.106.0 (nightly)"
        );
    }

    #[test]
    fn stable_binary_takes_the_nightly_of_its_version() {
        let m = manifest("v0.106.0", "20261027.b8d19ad7");
        assert!(nightly_is_update("v0.106.0", None, &m));
        assert!(nightly_is_update("v0.105.0", None, &m));
    }

    #[test]
    fn nightly_is_a_no_op_on_the_same_build() {
        let m = manifest("v0.106.0", "20261027.b8d19ad7");
        assert!(!nightly_is_update(
            "v0.106.0",
            Some("20261027.b8d19ad7"),
            &m
        ));
        assert!(nightly_is_update("v0.106.0", Some("20261026.0c1d2e3f"), &m));
    }

    #[test]
    fn nightly_never_moves_a_newer_binary_backwards() {
        let m = manifest("v0.106.0", "20261027.b8d19ad7");
        assert!(!nightly_is_update("v0.107.0", None, &m));
        assert!(!nightly_is_update(
            "v0.107.0",
            Some("20261101.aaaaaaaa"),
            &m
        ));
    }

    #[test]
    fn stable_updates_only_forward_for_stable_builds() {
        assert!(stable_is_update("v0.105.0", false, "v0.106.0"));
        assert!(!stable_is_update("v0.106.0", false, "v0.106.0"));
        assert!(!stable_is_update("v0.107.0", false, "v0.106.0"));
    }

    #[test]
    fn nightly_builds_move_to_the_stable_release_of_their_version() {
        assert!(stable_is_update("v0.106.0", true, "v0.106.0"));
        assert!(stable_is_update("v0.106.0", true, "v0.107.0"));
        // A nightly ahead of every release waits for the next one.
        assert!(!stable_is_update("v0.107.0", true, "v0.106.0"));
    }

    #[test]
    fn reads_the_manifest_from_a_signed_trusted_comment() {
        let m = NightlyManifest::from_signature(TEST_SIG).unwrap();
        assert_eq!(m, manifest("v0.106.0", "20261027.b8d19ad7"));
        assert_eq!(m.describe(), "v0.106.0 (nightly 20261027.b8d19ad7)");
        // The comment is covered by the signature the binary is checked with.
        crate::commands::upgrade::verify_release_signature(TEST_PUBKEY, TEST_SIG, TEST_PAYLOAD)
            .unwrap();
        let forged = with_comment("oak-nightly version=v0.106.0 build=20991231.ffffffff");
        assert!(crate::commands::upgrade::verify_release_signature(
            TEST_PUBKEY,
            &forged,
            TEST_PAYLOAD
        )
        .is_err());
    }

    #[test]
    fn manifest_ignores_unknown_keys_and_order() {
        let sig =
            with_comment("oak-nightly date=2026-10-27 build=20261027.b8d19ad7 version=v0.106.0");
        assert_eq!(
            NightlyManifest::from_signature(&sig).unwrap(),
            manifest("v0.106.0", "20261027.b8d19ad7")
        );
    }

    #[test]
    fn rejects_signatures_that_are_not_nightly_manifests() {
        // A stable release's signature: minisign's default comment.
        let stable = with_comment("timestamp:1780211915\tfile:oak-darwin-arm64\thashed");
        assert!(NightlyManifest::from_signature(&stable).is_err());
        let no_build = with_comment("oak-nightly version=v0.106.0");
        assert!(NightlyManifest::from_signature(&no_build).is_err());
        assert!(NightlyManifest::from_signature("not a signature").is_err());
    }
}
