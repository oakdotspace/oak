use std::fs;
use std::path::{Path, PathBuf};

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_REMOTE: &str = "https://oak.space";

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
    load_credentials_from_path(&path)
}

fn load_credentials_from_path(path: &Path) -> Result<Vec<Credential>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
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
    save_credential_to_path(&path, cred)
}

fn save_credential_to_path(path: &Path, cred: Credential) -> Result<()> {
    // Ensure ~/.oak/ directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut creds = load_credentials_from_path(path)?;

    // Replace existing credential for this server, or append
    if let Some(existing) = creds.iter_mut().find(|c| c.server == cred.server) {
        *existing = cred;
    } else {
        creds.push(cred);
    }

    let json = serde_json::to_string_pretty(&creds)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(path, json)?;

    // Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Remove the stored credential for a given server URL, rewriting the
/// credentials file. Returns whether an entry was actually removed (so the
/// caller can tell "logged out" from "wasn't logged in"). Other servers'
/// credentials are left untouched.
pub fn remove_credential(server: &str) -> Result<bool> {
    let path = credentials_path()?;
    let mut creds = load_credentials()?;

    let normalized = server.trim_end_matches('/');
    let before = creds.len();
    creds.retain(|c| c.server.trim_end_matches('/') != normalized);
    if creds.len() == before {
        return Ok(false);
    }

    let json = serde_json::to_string_pretty(&creds)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    fs::write(&path, json)?;

    // Preserve restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(true)
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

/// Resolve the default author identity for local commits and personal branch
/// names. `OAK_AUTHOR` is the explicit override; otherwise prefer the same
/// locally-stored account name that `oak whoami` prints for the default remote,
/// falling back to the OS user only when the user is not logged in.
pub fn preferred_author_name(fallback: &str) -> String {
    choose_author_name(
        std::env::var("OAK_AUTHOR").ok().as_deref(),
        get_username_for_server(DEFAULT_REMOTE).as_deref(),
        std::env::var("USER").ok().as_deref(),
        std::env::var("USERNAME").ok().as_deref(),
        fallback,
    )
}

fn choose_author_name(
    oak_author: Option<&str>,
    oak_username: Option<&str>,
    user: Option<&str>,
    username: Option<&str>,
    fallback: &str,
) -> String {
    [oak_author, oak_username, user, username]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
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
    use std::fs;

    use super::{choose_author_name, migrated_credential, save_credential_to_path, Credential};

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

    #[test]
    fn save_credential_to_path_preserves_invalid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        let original = "{this is not json";
        fs::write(&path, original).unwrap();

        let err = save_credential_to_path(&path, cred("https://oak.space", "tok-new"))
            .expect_err("invalid credentials file should prevent overwrite");

        assert!(err.to_string().contains("Invalid credentials file"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn author_prefers_explicit_oak_author() {
        assert_eq!(
            choose_author_name(
                Some("override"),
                Some("oak-user"),
                Some("machine"),
                Some("windows"),
                "fallback",
            ),
            "override"
        );
    }

    #[test]
    fn author_prefers_oak_whoami_over_machine_user() {
        assert_eq!(
            choose_author_name(None, Some("caviar"), Some("sanjayk"), None, "fallback"),
            "caviar"
        );
    }

    #[test]
    fn author_falls_back_to_machine_user_when_logged_out() {
        assert_eq!(
            choose_author_name(None, None, Some("sanjayk"), Some("winuser"), "fallback"),
            "sanjayk"
        );
    }

    #[test]
    fn author_ignores_empty_values() {
        assert_eq!(
            choose_author_name(
                Some("  "),
                Some(" caviar "),
                Some("sanjayk"),
                None,
                "fallback"
            ),
            "caviar"
        );
    }
}
