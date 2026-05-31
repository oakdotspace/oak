use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dialoguer::Confirm;
use minisign_verify::{PublicKey, Signature};
use oak_core::{OakError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::output;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Minisign public key that release binaries are signed against. The matching
/// secret key signs each binary at release time (`cli/Makefile`'s
/// `MINISIGN_SECKEY`) and is held OFF the release server — so even a fully
/// compromised release server cannot ship a binary that passes this check.
/// This is the real authenticity check; the SHA-256 verification just above it
/// only guards against corruption (the checksum comes from the same server as
/// the binary).
///
/// TODO(release): replace this with YOUR production public key before cutting
/// the first signed release. Generate a keypair once with `minisign -G` (keep
/// the `.key` secret key safe and out of the repo) and paste the base64 line
/// from the `.pub` file here. The value below is a throwaway dev key.
const RELEASE_PUBKEY: &str = "RWTQfszCQoIlhp/XG+dV3JXg8Yibl4e8ANvI0CgF2Ftar2OCf0JUb83E";

/// Verify a minisign `signature` over `data` against `pubkey_b64` (the bare
/// base64 line from a minisign `.pub` file). Returns an error if the key or
/// signature is malformed or the signature doesn't match.
fn verify_release_signature(pubkey_b64: &str, signature: &str, data: &[u8]) -> Result<()> {
    let public_key = PublicKey::from_base64(pubkey_b64)
        .map_err(|e| OakError::Server(format!("Invalid release public key: {e}")))?;
    let sig = Signature::decode(signature)
        .map_err(|e| OakError::Server(format!("Malformed release signature: {e}")))?;
    public_key
        .verify(data, &sig, false)
        .map_err(|e| OakError::Server(format!("Release signature verification failed: {e}")))
}

#[derive(Deserialize)]
struct LatestResponse {
    version: String,
}

/// Detect the current platform string
fn detect_platform() -> Result<&'static str> {
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

/// Run the upgrade command
pub async fn run(remote: &str, force: bool) -> Result<()> {
    let platform = detect_platform()?;
    let current_version = format!("v{VERSION}");

    output::info(&format!("Current version: {current_version}"));
    output::info(&format!("Platform: {platform}"));
    output::info("Checking for updates...");

    // Check latest version
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{remote}/api/releases/latest"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().as_u16() == 404 {
        output::info("No releases available on the server.");
        return Ok(());
    }

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(err_text));
    }

    let latest: LatestResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    output::info(&format!("Latest version: {}", latest.version));

    // Check if upgrade is needed
    if !is_newer_version(&current_version, &latest.version) {
        output::success("You are already running the latest version!");
        return Ok(());
    }

    println!();
    output::info(&format!(
        "A new version is available: {} -> {}",
        current_version, latest.version
    ));

    // Confirm upgrade
    if !force {
        let confirm = Confirm::new()
            .with_prompt("Do you want to upgrade?")
            .default(true)
            .interact()
            .map_err(|e| OakError::Server(e.to_string()))?;

        if !confirm {
            output::info("Upgrade cancelled.");
            return Ok(());
        }
    }

    // Get expected checksum
    output::info("Fetching checksum...");
    let checksum_resp = client
        .get(format!(
            "{}/api/releases/{}/{}/sha256",
            remote, latest.version, platform
        ))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !checksum_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Release {} not available for platform {}",
            latest.version, platform
        )));
    }

    let expected_sha256 = checksum_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    // Download the new binary
    output::info("Downloading new version...");
    let download_resp = client
        .get(format!(
            "{}/api/releases/{}/{}",
            remote, latest.version, platform
        ))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !download_resp.status().is_success() {
        return Err(OakError::Server("Failed to download release".to_string()));
    }

    let binary_data = download_resp
        .bytes()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    // Verify checksum
    output::info("Verifying checksum...");
    let mut hasher = Sha256::new();
    hasher.update(&binary_data);
    let actual_sha256 = hex::encode(hasher.finalize());

    if actual_sha256 != expected_sha256 {
        return Err(OakError::Server(format!(
            "Checksum mismatch! Expected: {expected_sha256}, Got: {actual_sha256}"
        )));
    }

    output::success("Checksum verified!");

    // Verify the minisign signature over the downloaded binary. This is the
    // authenticity check (the checksum above only catches corruption, since it
    // comes from the same server). The signing key lives off the release
    // server, so this is what stops a compromised server from shipping a forged
    // binary. Fail closed: an unsigned release or a bad signature aborts.
    output::info("Verifying signature...");
    let sig_resp = client
        .get(format!(
            "{}/api/releases/{}/{}/minisig",
            remote, latest.version, platform
        ))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !sig_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Release {} for {} has no signature on the server — refusing to upgrade.",
            latest.version, platform
        )));
    }

    let signature = sig_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    verify_release_signature(RELEASE_PUBKEY, &signature, &binary_data)?;
    output::success("Signature verified!");

    // Get current executable path
    let current_exe = env::current_exe().map_err(OakError::Io)?;

    // Create a backup path
    let backup_path = current_exe.with_extension("old");

    // Write to a temporary file first
    let temp_path = current_exe.with_extension("new");

    output::info("Installing new version...");

    // Write the new binary
    fs::write(&temp_path, &binary_data)?;

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

    println!();
    output::success(&format!(
        "Successfully upgraded oak from {} to {}!",
        current_version, latest.version
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
}
