use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dialoguer::Confirm;
use minisign_verify::{PublicKey, Signature};
use oak_core::{OakError, Result};

use crate::commands::update_channel::{self, Channel};
use crate::output;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Minisign public key that release binaries are signed against. The matching
/// secret key signs each binary at release time (the root `Makefile`'s
/// `sign-release`, run by the Oak CI release workflows with the
/// `MINISIGN_SECKEY` environment secret) and is held OFF the distribution
/// server — it never touches oak.space. So even if a served release is
/// tampered with, a forged binary cannot pass this check. This signature is the
/// integrity AND authenticity check for `oak upgrade`.
///
/// Production key, generated passwordless with `minisign -G -W` (its secret half
/// lives only in the `MINISIGN_SECKEY` release secret). To rotate: generate a
/// new keypair, update `MINISIGN_SECKEY`, and ship a normal signed release with
/// the new value here — clients on the old binary keep verifying against the
/// old key until they upgrade.
pub(crate) const RELEASE_PUBKEY: &str = "RWQvvXyHZKQ0v5iV/R3Vb+kNbG+dz2Ttu+OWJ493mIhtaXRequka1FhR";

/// Verify a minisign `signature` over `data` against `pubkey_b64` (the bare
/// base64 line from a minisign `.pub` file). Returns an error if the key or
/// signature is malformed or the signature doesn't match.
pub(crate) fn verify_release_signature(
    pubkey_b64: &str,
    signature: &str,
    data: &[u8],
) -> Result<()> {
    let public_key = PublicKey::from_base64(pubkey_b64)
        .map_err(|e| OakError::Server(format!("Invalid release public key: {e}")))?;
    let sig = Signature::decode(signature)
        .map_err(|e| OakError::Server(format!("Malformed release signature: {e}")))?;
    public_key
        .verify(data, &sig, false)
        .map_err(|e| OakError::Server(format!("Release signature verification failed: {e}")))
}

/// Base URL of the oak.space release API every `oak upgrade` path, the daily
/// update check and the macOS mounter download read: `OAK_URL`, default
/// `https://oak.space` (the knob `install.sh` honors too).
pub(crate) fn release_base_url() -> String {
    env::var("OAK_URL")
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| "https://oak.space".to_string())
}

/// The `platform` binary of release `version` (`v0.106.0`, or
/// [`update_channel::NIGHTLY_VERSION`]). Its signature is at `<url>/minisig`.
pub(crate) fn release_url(version: &str, platform: &str) -> String {
    format!("{}/api/releases/{version}/{platform}", release_base_url())
}

/// Client for the release API. Carries the standard CLI User-Agent —
/// Cloudflare's bot rules reject UA-less requests.
fn download_client() -> Result<reqwest::Client> {
    crate::http::ensure_crypto_provider();
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()
        .map_err(|e| OakError::Http(e.to_string()))
}

/// `GET /api/releases/latest`. Unknown fields are ignored.
#[derive(serde::Deserialize)]
struct LatestRelease {
    version: String,
}

fn parse_latest_version(body: &str) -> Result<String> {
    let latest: LatestRelease = serde_json::from_str(body)
        .map_err(|e| OakError::Server(format!("Malformed latest-release response: {e}")))?;
    if latest.version.is_empty() {
        return Err(OakError::Server(
            "Malformed latest-release response: empty version".to_string(),
        ));
    }
    Ok(latest.version)
}

/// The newest stable release (`v0.106.0`), from oak.space's
/// `/api/releases/latest`: the lookup `install.sh` makes too, so the
/// installer, `oak upgrade` and the daily check always agree on "latest". Only
/// promoted releases count; staged ones and the nightly never show up here.
/// Returns `Ok(None)` when nothing has been released.
pub(crate) async fn fetch_latest_version(client: &reqwest::Client) -> Result<Option<String>> {
    let resp = client
        .get(format!("{}/api/releases/latest", release_base_url()))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(OakError::Server(format!(
            "Unexpected response resolving latest release: HTTP {status}"
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    parse_latest_version(&body).map(Some)
}

/// Detect the current platform string
pub(crate) fn detect_platform() -> Result<&'static str> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;

    match (os, arch) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x86_64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("windows", "x86_64") => Ok("windows-x86_64"),
        _ => Err(OakError::Server(format!(
            "Unsupported platform: {os}-{arch}"
        ))),
    }
}

/// Compare version strings (assumes semver format vX.Y.Z)
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    let parse_version = |v: &str| -> (u32, u32, u32) {
        let v = v.trim_start_matches('v');
        let parts: Vec<&str> = v.split('.').collect();
        let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };

    let current_v = parse_version(current);
    let latest_v = parse_version(latest);

    latest_v > current_v
}

/// Download release `version`'s binary for `platform` and its `.minisig`,
/// verify the signature against [`RELEASE_PUBKEY`], and return the verified
/// bytes.
///
/// The signing key lives off the distribution server, so a valid signature is
/// what proves the bytes are a genuine Oak release. Fails closed — a missing or
/// bad signature aborts. Used by the stable path; the nightly paths check
/// against the signature they already read (see [`download_nightly`]).
async fn download_and_verify(
    client: &reqwest::Client,
    version: &str,
    platform: &str,
) -> Result<Vec<u8>> {
    let url = release_url(version, platform);
    let binary_data = download_binary(client, &url, version, platform).await?;

    output::info("Verifying signature...");
    let sig_resp = client
        .get(format!("{url}/minisig"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !sig_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Release {version} for {platform} has no signature — refusing to upgrade."
        )));
    }

    let signature = sig_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    verify_release_signature(RELEASE_PUBKEY, &signature, &binary_data)?;
    output::success("Signature verified!");

    Ok(binary_data)
}

/// Download the current nightly binary for `platform` and verify it against
/// `signature`, which the caller fetched first to read the build from.
async fn download_nightly(
    client: &reqwest::Client,
    platform: &str,
    signature: &str,
) -> Result<Vec<u8>> {
    let binary_data = download_binary(
        client,
        &release_url(update_channel::NIGHTLY_VERSION, platform),
        update_channel::NIGHTLY_VERSION,
        platform,
    )
    .await?;
    output::info("Verifying signature...");
    // The signature was fetched before the binary, so a nightly published in
    // between fails here too.
    verify_release_signature(RELEASE_PUBKEY, signature, &binary_data).map_err(|e| match e {
        OakError::Server(msg) => OakError::Server(format!(
            "{msg}. If a new nightly was published while this ran, run `oak upgrade` again."
        )),
        other => other,
    })?;
    output::success("Signature verified!");
    Ok(binary_data)
}

/// Download the binary of `release` for `platform` from `url`.
async fn download_binary(
    client: &reqwest::Client,
    url: &str,
    release: &str,
    platform: &str,
) -> Result<Vec<u8>> {
    output::info("Downloading new version...");
    let download_resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !download_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Failed to download release {release} for platform {platform} (HTTP {})",
            download_resp.status()
        )));
    }

    Ok(download_resp
        .bytes()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?
        .to_vec())
}

/// Atomically swap `binary_data` into place of the currently running executable,
/// keeping a `.old` backup until the rename succeeds (and restoring it on
/// failure). The bytes must already be signature-verified.
fn install_in_place(binary_data: &[u8]) -> Result<()> {
    let current_exe = env::current_exe().map_err(OakError::Io)?;
    let backup_path = current_exe.with_extension("old");
    let temp_path = current_exe.with_extension("new");

    output::info("Installing new version...");
    fs::write(&temp_path, binary_data)?;

    // Make it executable (unix only)
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&temp_path, perms)?;
    }

    // Backup current binary
    if current_exe.exists() {
        fs::rename(&current_exe, &backup_path)?;
    }

    // Move new binary into place
    if let Err(e) = fs::rename(&temp_path, &current_exe) {
        // Try to restore backup
        if backup_path.exists() {
            let _ = fs::rename(&backup_path, &current_exe);
        }
        return Err(OakError::Io(e));
    }

    // Remove backup
    let _ = fs::remove_file(&backup_path);
    Ok(())
}

/// Prompt for confirmation unless `force`. Returns `true` to proceed.
fn confirm_upgrade(force: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    let confirm = Confirm::new()
        .with_prompt("Do you want to upgrade?")
        .default(true)
        .interact()
        .map_err(|e| OakError::Server(e.to_string()))?;
    if !confirm {
        output::info("Upgrade cancelled.");
    }
    Ok(confirm)
}

/// Run the upgrade command. Every release comes from oak.space (see
/// [`release_base_url`]); the downloaded binary is verified against its
/// `.minisig` before being swapped into place. `channel`, when given, is saved
/// as the channel to follow from now on (see [`update_channel`]); otherwise
/// the saved one is used. With `canary`, installs the current nightly build
/// once, leaving the saved channel alone.
pub async fn run(force: bool, canary: bool, channel: Option<Channel>) -> Result<()> {
    let platform = detect_platform()?;
    let current_version = format!("v{VERSION}");

    output::info(&format!(
        "Current version: {}",
        update_channel::describe_current()
    ));
    output::info(&format!("Platform: {platform}"));

    if canary {
        return run_canary(platform, force).await;
    }

    let channel = match channel {
        Some(channel) => {
            let previous = update_channel::selected();
            update_channel::select(channel)?;
            if previous != channel {
                output::success(&format!(
                    "Update channel set to {} (`oak upgrade` and the daily update notice follow it from now on).",
                    channel.as_str()
                ));
            }
            channel
        }
        None => update_channel::selected(),
    };
    output::info(&format!("Channel: {}", channel.as_str()));

    match channel {
        Channel::Stable => run_stable(platform, force, &current_version).await,
        Channel::Nightly => run_nightly(platform, force, &current_version).await,
    }
}

async fn run_stable(platform: &str, force: bool, current_version: &str) -> Result<()> {
    output::info("Checking for updates...");

    let client = download_client()?;
    let latest_version = match fetch_latest_version(&client).await? {
        Some(version) => version,
        None => {
            output::info("No releases available.");
            return Ok(());
        }
    };

    output::info(&format!("Latest version: {latest_version}"));

    let running_nightly = update_channel::running_nightly_build().is_some();
    if !update_channel::stable_is_update(current_version, running_nightly, &latest_version) {
        if running_nightly {
            output::info(&format!(
                "This nightly build is ahead of the latest stable release; `oak upgrade` moves you to stable once a release newer than {current_version} ships."
            ));
        } else {
            output::success("You are already running the latest version!");
        }
        return Ok(());
    }

    let current = update_channel::describe_current();
    output::print_line("");
    output::info(&format!(
        "A new version is available: {current} -> {latest_version}"
    ));

    if !confirm_upgrade(force)? {
        return Ok(());
    }

    let binary_data = download_and_verify(&client, &latest_version, platform).await?;
    install_in_place(&binary_data)?;

    output::print_line("");
    output::success(&format!(
        "Successfully upgraded oak from {current} to {latest_version}!"
    ));

    Ok(())
}

/// Nightly path: the rolling [`update_channel::NIGHTLY_VERSION`] on oak.space.
/// Its signature names the build, so the up-to-date check needs no binary
/// download, and the binary is then verified against that same signature.
async fn run_nightly(platform: &str, force: bool, current_version: &str) -> Result<()> {
    output::info("Checking for nightly builds...");

    let client = download_client()?;
    let signature = update_channel::fetch_nightly_signature(&client, platform).await?;
    let manifest = update_channel::NightlyManifest::from_signature(&signature)?;
    let latest = manifest.describe();
    output::info(&format!("Latest nightly: {latest}"));

    let current_build = update_channel::running_nightly_build();
    if !update_channel::nightly_is_update(current_version, current_build, &manifest) {
        if current_build == Some(manifest.build.as_str()) {
            output::success("You are already running the latest nightly build!");
        } else {
            output::info(&format!(
                "{current_version} is newer than the latest nightly build; nothing to install."
            ));
        }
        return Ok(());
    }

    let current = update_channel::describe_current();
    output::print_line("");
    output::info(&format!(
        "A new nightly build is available: {current} -> {latest}"
    ));

    if !confirm_upgrade(force)? {
        return Ok(());
    }

    let binary_data = download_nightly(&client, platform, &signature).await?;
    install_in_place(&binary_data)?;

    output::print_line("");
    output::success(&format!(
        "Successfully upgraded oak from {current} to {latest}!"
    ));

    Ok(())
}

/// Canary path: install the current nightly build once, whatever the saved
/// channel, and leave the channel alone (so a later `oak upgrade` returns to
/// it). Canary builds used to be a separate GitHub pre-release built on every
/// push; the nightly is now the only pre-release build, and deploying the
/// `nightly` environment publishes a fresh one of `main` at any time.
async fn run_canary(platform: &str, force: bool) -> Result<()> {
    output::info(
        "Canary: installing the current nightly build once; your update channel doesn't change.",
    );

    let client = download_client()?;
    let signature = update_channel::fetch_nightly_signature(&client, platform).await?;
    let manifest = update_channel::NightlyManifest::from_signature(&signature)?;
    let latest = manifest.describe();
    output::info(&format!("Latest nightly: {latest}"));

    if update_channel::running_nightly_build() == Some(manifest.build.as_str()) {
        output::success("Already running the latest nightly build.");
        return Ok(());
    }

    let current = update_channel::describe_current();
    output::print_line("");
    output::info(&format!("Install {latest} (now {current})?"));

    if !confirm_upgrade(force)? {
        return Ok(());
    }

    let binary_data = download_nightly(&client, platform, &signature).await?;
    install_in_place(&binary_data)?;

    output::print_line("");
    output::success(&format!(
        "Installed {latest}. `oak upgrade` returns you to the {} channel.",
        update_channel::selected().as_str()
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real minisign keypair + signature produced with `rsign` (minisign-
    // compatible). Signed payload is exactly b"hello-oak-minisign\n". These
    // pin down that verify_release_signature accepts a genuine signature and
    // rejects tampered data — independent of the production RELEASE_PUBKEY.
    const TEST_PUBKEY: &str = "RWTQfszCQoIlhp/XG+dV3JXg8Yibl4e8ANvI0CgF2Ftar2OCf0JUb83E";
    const TEST_SIG: &str = "untrusted comment: signature from rsign secret key\n\
RUTQfszCQoIlhpRZsuBtFg5Cb2qOeYcydBkDdO5YKHGf8/T6sKuZG4ttlPEKSRcAIm5SrqyG14fVF5gUBayZBlaJIJhFxH5hXwA=\n\
trusted comment: timestamp:1780211915\tfile:testfile\tprehashed\n\
jxJE7ZGdfjP6hE9SCW/HPmkcGivAlIeMIZLWuQIlhLko+CuS6at3uYq6HeFkYquKaZoZps2P1tjOBwE+ZIpBBg==\n";
    const TEST_DATA: &[u8] = b"hello-oak-minisign\n";

    #[test]
    fn accepts_valid_signature() {
        verify_release_signature(TEST_PUBKEY, TEST_SIG, TEST_DATA)
            .expect("a genuine signature over the exact payload must verify");
    }

    #[test]
    fn rejects_tampered_data() {
        assert!(verify_release_signature(TEST_PUBKEY, TEST_SIG, b"tampered payload").is_err());
    }

    #[test]
    fn rejects_wrong_key() {
        // Flip the key: a different (valid-format) pubkey must not verify.
        let other = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert!(verify_release_signature(other, TEST_SIG, TEST_DATA).is_err());
    }

    #[test]
    fn rejects_malformed_signature() {
        assert!(verify_release_signature(TEST_PUBKEY, "not a signature", TEST_DATA).is_err());
    }

    #[test]
    fn reads_the_latest_version() {
        assert_eq!(
            parse_latest_version(r#"{"version":"v0.106.0"}"#).unwrap(),
            "v0.106.0"
        );
        // Fields a newer server adds are ignored.
        assert_eq!(
            parse_latest_version(r#"{"version":"v0.107.0","promoted_at":"2026-11-01T00:00:00Z"}"#)
                .unwrap(),
            "v0.107.0"
        );
    }

    #[test]
    fn rejects_malformed_latest_responses() {
        assert!(parse_latest_version("<html>").is_err());
        assert!(parse_latest_version(r#"{"error":"No releases found"}"#).is_err());
        assert!(parse_latest_version(r#"{"version":""}"#).is_err());
    }
}
