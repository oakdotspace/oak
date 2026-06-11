use std::fs;
use std::path::PathBuf;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub server: String,
    pub token: String,
    pub username: String,
}

/// Get the path to the global credentials file (~/.oak/credentials)
pub fn credentials_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| OakError::Io(std::io::Error::other("Could not determine home directory")))?;
    Ok(home.join(".oak").join("credentials"))
}

/// Load all stored credentials
pub fn load_credentials() -> Result<Vec<Credential>> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&path)?;
    let creds: Vec<Credential> = serde_json::from_str(&contents).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "Invalid credentials file: {e}"
        )))
    })?;
    Ok(creds)
}

/// Save credentials, replacing any existing entry for the same server
pub fn save_credential(cred: Credential) -> Result<()> {
    let path = credentials_path()?;

    // Ensure ~/.oak/ directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut creds = load_credentials().unwrap_or_default();

    // Replace existing credential for this server, or append
    if let Some(existing) = creds.iter_mut().find(|c| c.server == cred.server) {
        *existing = cred;
    } else {
        creds.push(cred);
    }

    let json = serde_json::to_string_pretty(&creds)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, json)?;

    // Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Get the token for a given server URL
pub fn get_token_for_server(server: &str) -> Option<String> {
    let creds = load_credentials().ok()?;
    // Normalize: strip trailing slash for comparison
    let normalized = server.trim_end_matches('/');
    creds
        .into_iter()
        .find(|c| c.server.trim_end_matches('/') == normalized)
        .map(|c| c.token)
}

/// Get the logged-in username for a given server URL
pub fn get_username_for_server(server: &str) -> Option<String> {
    let creds = load_credentials().ok()?;
    let normalized = server.trim_end_matches('/');
    creds
        .into_iter()
        .find(|c| c.server.trim_end_matches('/') == normalized)
        .map(|c| c.username)
}

/// Copy the stored credential for `old_server` to `new_server`, so a host
/// move (the old origin redirecting to a new one) keeps the user logged in —
/// tokens are keyed by server URL, so without this `get_token_for_server`
/// misses the old host's token after the remote is retargeted. The old
/// entry is kept (harmless, and the old host may still serve other repos).
/// Returns whether a credential was written.
pub fn migrate_server_credential(old_server: &str, new_server: &str) -> Result<bool> {
    let creds = load_credentials().unwrap_or_default();
    match migrated_credential(&creds, old_server, new_server) {
        Some(cred) => {
            save_credential(cred)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Pure core of [`migrate_server_credential`]: the credential to store for
/// `new_server`, or `None` when there's nothing to migrate — no token for
/// the old host, or the new host already has its own (never overwrite a
/// real login with a copied one).
fn migrated_credential(
    creds: &[Credential],
    old_server: &str,
    new_server: &str,
) -> Option<Credential> {
    let new_normalized = new_server.trim_end_matches('/');
    if creds
        .iter()
        .any(|c| c.server.trim_end_matches('/') == new_normalized)
    {
        return None;
    }
    let old_normalized = old_server.trim_end_matches('/');
    let old = creds
        .iter()
        .find(|c| c.server.trim_end_matches('/') == old_normalized)?;
    Some(Credential {
        server: new_server.to_string(),
        token: old.token.clone(),
        username: old.username.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{migrated_credential, Credential};

    fn cred(server: &str, token: &str) -> Credential {
        Credential {
            server: server.to_string(),
            token: token.to_string(),
            username: "tester".to_string(),
        }
    }

    #[test]
    fn copies_old_token_to_new_server() {
        let creds = [cred("https://oakvcs.com", "tok-old")];
        let migrated =
            migrated_credential(&creds, "https://oakvcs.com", "https://oak.space").unwrap();
        assert_eq!(migrated.server, "https://oak.space");
        assert_eq!(migrated.token, "tok-old");
        assert_eq!(migrated.username, "tester");
    }

    #[test]
    fn trailing_slashes_do_not_defeat_matching() {
        let creds = [cred("https://oakvcs.com/", "tok-old")];
        assert!(migrated_credential(&creds, "https://oakvcs.com", "https://oak.space").is_some());
    }

    #[test]
    fn no_old_credential_means_nothing_to_migrate() {
        assert!(migrated_credential(&[], "https://oakvcs.com", "https://oak.space").is_none());
    }

    #[test]
    fn existing_new_credential_is_never_overwritten() {
        let creds = [
            cred("https://oakvcs.com", "tok-old"),
            cred("https://oak.space", "tok-new"),
        ];
        assert!(migrated_credential(&creds, "https://oakvcs.com", "https://oak.space").is_none());
    }
}
