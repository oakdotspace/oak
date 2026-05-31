use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dialoguer::Confirm;
use oak_core::{OakError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::output;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
