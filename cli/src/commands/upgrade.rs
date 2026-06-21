use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dialoguer::Confirm;
use minisign_verify::{PublicKey, Signature};
use oak_core::{OakError, Result};

use crate::output;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Minisign public key that release binaries are signed against. The matching
/// secret key signs each binary at release time (the root `Makefile`'s
/// `sign-release`, driven by the `MINISIGN_SECKEY` GitHub Actions secret) and is
/// held OFF the distribution channel — it never touches GitHub. So even if the
/// GitHub Release is tampered with, a forged binary cannot pass this check. This
/// signature is the integrity AND authenticity check for `oak upgrade`.
///
/// Production key, generated passwordless with `minisign -G -W` (its secret half
/// lives only in the `MINISIGN_SECKEY` GitHub Actions secret). To rotate:
/// generate a new keypair, update `MINISIGN_SECKEY`, and ship a normal signed
/// release with the new value here — clients on the old binary keep verifying
/// against the old key until they upgrade.
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

/// GitHub repo that hosts `oak` releases (`owner/name`). Releases are published
/// here — the binary, its `.minisig`, and `SHA256SUMS` are attached as release
/// assets. Overridable via `OAK_RELEASE_REPO` for forks / self-hosting / tests.
const RELEASE_REPO: &str = "oakdotspace/oak";

fn release_repo() -> String {
    env::var("OAK_RELEASE_REPO").unwrap_or_else(|_| RELEASE_REPO.to_string())
}

/// Fixed tag of the rolling canary pre-release. Unlike a stable `vX.Y.Z` tag,
/// this single tag is force-moved to the tip of the `oak` branch on every push
/// (see `.github/workflows/canary.yml`), so its assets always hold the latest
/// canary build. It is published as a GitHub *pre-release*, which is why the
/// stable path's `/releases/latest` redirect never resolves to it — stable
/// users only ever see tagged releases. `oak upgrade --canary` opts in by
/// addressing this tag directly.
const CANARY_TAG: &str = "canary";

/// Pull the release tag (e.g. `v0.95.0`) out of the `Location` URL that GitHub's
/// `/releases/latest` redirect points at
/// (`https://github.com/<repo>/releases/tag/v0.95.0`). Returns `None` if the URL
/// isn't a release-tag link.
fn parse_tag_from_location(location: &str) -> Option<String> {
    // Drop any query/fragment, trim a trailing slash, then take the segment
    // after `/releases/tag/`.
    let path = location
        .split(['?', '#'])
        .next()
        .unwrap_or(location)
        .trim_end_matches('/');
    let (_, tag) = path.rsplit_once("/releases/tag/")?;
    if tag.is_empty() || tag.contains('/') {
        None
    } else {
        Some(tag.to_string())
    }
}

/// Redirect-following client for fetching release assets (GitHub serves
/// them via a redirect to its object store). Carries the standard CLI
/// User-Agent — Cloudflare's bot rules reject UA-less requests.
fn download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()
        .map_err(|e| OakError::Http(e.to_string()))
}

/// Resolve the latest published release tag by following GitHub's
/// `/releases/latest` redirect. `client` MUST have redirect-following **disabled**
/// so the `Location` header (pointing at `.../releases/tag/<tag>`) is readable.
/// Returns `Ok(None)` when the repo has no published, non-prerelease release.
///
/// Uses the web redirect rather than `api.github.com` deliberately: it needs no
/// auth and doesn't count against the unauthenticated API rate limit, so the
/// once-a-day version check stays reliable even behind shared/NAT'd IPs.
pub(crate) async fn fetch_latest_tag(client: &reqwest::Client) -> Result<Option<String>> {
    let repo = release_repo();
    let resp = client
        .get(format!("https://github.com/{repo}/releases/latest"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                OakError::Server(
                    "GitHub latest-release redirect had no Location header".to_string(),
                )
            })?;
        Ok(parse_tag_from_location(location))
    } else if status.as_u16() == 404 {
        // Repo has no published release yet.
        Ok(None)
    } else {
        Err(OakError::Server(format!(
            "Unexpected response resolving latest release: HTTP {status}"
        )))
    }
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

/// Download the `oak-<platform>` binary for `tag` and its `.minisig`, verify the
/// signature against [`RELEASE_PUBKEY`], and return the verified bytes.
///
/// With the binary and its signature both served from GitHub, this is the
/// integrity AND authenticity check: the signing key lives off the distribution
/// channel (it never touches GitHub), so a valid signature is what proves the
/// bytes are a genuine Oak release. Fails closed — a missing or bad signature
/// aborts. Used by both the stable and canary paths (the only difference between
/// them is which `tag` they address).
async fn download_and_verify(
    client: &reqwest::Client,
    tag: &str,
    platform: &str,
) -> Result<Vec<u8>> {
    let repo = release_repo();
    let binary_url = format!("https://github.com/{repo}/releases/download/{tag}/oak-{platform}");

    // Asset downloads follow redirects (GitHub serves release assets via a
    // redirect to its object store).
    output::info("Downloading new version...");
    let download_resp = client
        .get(&binary_url)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !download_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Failed to download release {tag} for platform {platform} (HTTP {})",
            download_resp.status()
        )));
    }

    let binary_data = download_resp
        .bytes()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    output::info("Verifying signature...");
    let sig_resp = client
        .get(format!("{binary_url}.minisig"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !sig_resp.status().is_success() {
        return Err(OakError::Server(format!(
            "Release {tag} for {platform} has no signature — refusing to upgrade."
        )));
    }

    let signature = sig_resp
        .text()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    verify_release_signature(RELEASE_PUBKEY, &signature, &binary_data)?;
    output::success("Signature verified!");

    Ok(binary_data.to_vec())
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

/// Run the upgrade command. Releases are pulled from GitHub Releases (see
/// [`RELEASE_REPO`]); the downloaded binary is verified against its `.minisig`
/// before being swapped into place. With `canary`, tracks the rolling
/// [`CANARY_TAG`] pre-release instead of the latest stable release.
pub async fn run(force: bool, canary: bool) -> Result<()> {
    let platform = detect_platform()?;
    let current_version = format!("v{VERSION}");

    output::info(&format!("Current version: {current_version}"));
    output::info(&format!("Platform: {platform}"));

    if canary {
        return run_canary(platform, force).await;
    }

    output::info("Checking for updates...");

    // Latest-version lookup needs redirects DISABLED so we can read the
    // `Location` header off GitHub's `/releases/latest` 302.
    let latest_client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| OakError::Http(e.to_string()))?;

    let latest_version = match fetch_latest_tag(&latest_client).await? {
        Some(tag) => tag,
        None => {
            output::info("No releases available.");
            return Ok(());
        }
    };

    output::info(&format!("Latest version: {latest_version}"));

    // Check if upgrade is needed
    if !is_newer_version(&current_version, &latest_version) {
        output::success("You are already running the latest version!");
        return Ok(());
    }

    println!();
    output::info(&format!(
        "A new version is available: {current_version} -> {latest_version}"
    ));

    if !confirm_upgrade(force)? {
        return Ok(());
    }

    let client = download_client()?;
    let binary_data = download_and_verify(&client, &latest_version, platform).await?;
    install_in_place(&binary_data)?;

    println!();
    output::success(&format!(
        "Successfully upgraded oak from {current_version} to {latest_version}!"
    ));

    Ok(())
}

/// Canary path: address the fixed [`CANARY_TAG`] pre-release directly (its
/// `/releases/latest` exclusion is exactly why stable users never see it). The
/// tag is rolling, so there's no `vX.Y.Z` to compare — instead we fetch the
/// current canary binary and short-circuit only if its bytes are identical to
/// the running executable.
async fn run_canary(platform: &str, force: bool) -> Result<()> {
    output::info("Channel: canary (rolling pre-release, tracks the tip of the oak branch)");

    let client = download_client()?;
    let binary_data = download_and_verify(&client, CANARY_TAG, platform).await?;

    // No semver to compare against a rolling tag — compare the bytes instead so
    // a re-run that's already on the current canary is a clean no-op.
    let current_exe = env::current_exe().map_err(OakError::Io)?;
    if let Ok(existing) = fs::read(&current_exe) {
        if existing.as_slice() == binary_data.as_slice() {
            output::success("Already running the latest canary build.");
            return Ok(());
        }
    }

    println!();
    output::info("A newer canary build is available.");

    if !confirm_upgrade(force)? {
        return Ok(());
    }

    install_in_place(&binary_data)?;

    println!();
    output::success("Successfully upgraded oak to the latest canary build!");

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
    fn parses_tag_from_release_location() {
        assert_eq!(
            parse_tag_from_location("https://github.com/oakdotspace/oak/releases/tag/v0.95.0"),
            Some("v0.95.0".to_string())
        );
        // Trailing slash, query, and fragment are all tolerated.
        assert_eq!(
            parse_tag_from_location("https://github.com/oakdotspace/oak/releases/tag/v1.2.3/"),
            Some("v1.2.3".to_string())
        );
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/oakdotspace/oak/releases/tag/v1.2.3?foo=bar"
            ),
            Some("v1.2.3".to_string())
        );
    }

    #[test]
    fn rejects_non_release_location() {
        // A repo with no releases redirects to /releases, not /releases/tag/<tag>.
        assert_eq!(
            parse_tag_from_location("https://github.com/oakdotspace/oak/releases"),
            None
        );
        assert_eq!(parse_tag_from_location("https://example.com/"), None);
        // No tag segment after /tag/.
        assert_eq!(
            parse_tag_from_location("https://github.com/oakdotspace/oak/releases/tag/"),
            None
        );
    }
}
