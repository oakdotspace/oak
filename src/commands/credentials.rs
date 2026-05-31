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
