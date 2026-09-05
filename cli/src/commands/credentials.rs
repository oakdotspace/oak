use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use oak_core::{MetadataKey, OakError, Repository, Result};

/// Resolve one normalized credential for a remote. Blank/whitespace process
/// values are unset, and every network command shares this precedence rule so
/// clone proof, pull, push, mount, and agent workflows cannot act as different
/// principals accidentally.
pub fn effective_token(remote: &str, repository_token: Option<String>) -> Option<String> {
    resolve_token(
        std::env::var("OAK_API_KEY").ok(),
        repository_token,
        get_token_for_server(remote),
    )
}

/// Resolve the credential for a command operating on an already-open local
/// repository. This is the canonical repository-aware seam: process override,
/// then repository metadata, then the account credential for the remote.
pub fn effective_token_for_repository(remote: &str, repo: &dyn Repository) -> Option<String> {
    effective_token(
        remote,
        repo.get_metadata(MetadataKey::ApiKey).ok().flatten(),
    )
}

/// Resolve a repository-aware credential when a command starts from a working
/// path. Non-repository/account-wide commands deliberately keep using
/// [`effective_token`] with no repository token.
pub fn effective_token_for_worktree(remote: &str, work_path: &Path) -> Option<String> {
    let repository = crate::resolve::resolve(work_path)
        .ok()
        .and_then(|context| context.open().ok());
    match repository {
        Some(repository) => effective_token_for_repository(remote, repository.as_ref()),
        None => effective_token(remote, None),
    }
}

fn resolve_token(
    environment: Option<String>,
    repository: Option<String>,
    stored: Option<String>,
) -> Option<String> {
    [environment, repository, stored]
        .into_iter()
        .flatten()
        .map(|token| token.trim().to_string())
        .find(|token| !token.is_empty())
}
use serde::{Deserialize, Serialize};

use crate::atomic_file;

pub const DEFAULT_REMOTE: &str = "https://oak.space";
const CREDENTIAL_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_CREDENTIAL_LOCK_AGE: Duration = Duration::from_secs(30);

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
    let _lock = CredentialFileLock::acquire_for(path)?;

    let mut creds = load_credentials_from_path(path)?;
    pause_after_credentials_load_for_tests();

    // Replace existing credential for this server, or append
    if let Some(existing) = creds.iter_mut().find(|c| c.server == cred.server) {
        *existing = cred;
    } else {
        creds.push(cred);
    }

    let json = serde_json::to_string_pretty(&creds)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    atomic_file::write_atomic_private(path, json)?;

    Ok(())
}

/// Remove the stored credential for a given server URL, rewriting the
/// credentials file. Returns whether an entry was actually removed (so the
/// caller can tell "logged out" from "wasn't logged in"). Other servers'
/// credentials are left untouched.
pub fn remove_credential(server: &str) -> Result<bool> {
    let path = credentials_path()?;
    remove_credential_from_path(&path, server)
}

fn remove_credential_from_path(path: &Path, server: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let _lock = CredentialFileLock::acquire_for(path)?;
    let mut creds = load_credentials_from_path(path)?;

    let normalized = server.trim_end_matches('/');
    let before = creds.len();
    creds.retain(|c| c.server.trim_end_matches('/') != normalized);
    if creds.len() == before {
        return Ok(false);
    }

    let json = serde_json::to_string_pretty(&creds)
        .map_err(|e| OakError::Io(std::io::Error::other(e.to_string())))?;
    atomic_file::write_atomic_private(path, json)?;

    Ok(true)
}

struct CredentialFileLock {
    lock_path: PathBuf,
}

impl CredentialFileLock {
    fn acquire_for(credentials_path: &Path) -> Result<Self> {
        let lock_path = credential_lock_path(credentials_path)?;
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let start = Instant::now();
        loop {
            match create_lock_file(&lock_path) {
                Ok(mut file) => {
                    let pid = std::process::id();
                    if let Err(e) = writeln!(file, "{pid}") {
                        let _ = fs::remove_file(&lock_path);
                        return Err(OakError::Io(e));
                    }
                    if let Err(e) = file.sync_all() {
                        let _ = fs::remove_file(&lock_path);
                        return Err(OakError::Io(e));
                    }
                    return Ok(Self { lock_path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if credential_lock_is_stale(&lock_path)? {
                        match fs::remove_file(&lock_path) {
                            Ok(()) => continue,
                            Err(e) if e.kind() == ErrorKind::NotFound => continue,
                            Err(e) => return Err(OakError::Io(e)),
                        }
                    }
                    if start.elapsed() >= CREDENTIAL_LOCK_TIMEOUT {
                        return Err(OakError::Io(io::Error::new(
                            ErrorKind::TimedOut,
                            "credentials file is locked by another process",
                        )));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(OakError::Io(e)),
            }
        }
    }
}

impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        if let Ok(contents) = fs::read_to_string(&self.lock_path) {
            if contents.trim() == std::process::id().to_string() {
                let _ = fs::remove_file(&self.lock_path);
            }
        }
    }
}

fn credential_lock_path(credentials_path: &Path) -> Result<PathBuf> {
    let file_name = credentials_path.file_name().ok_or_else(|| {
        OakError::Io(io::Error::new(
            ErrorKind::InvalidInput,
            "credentials path has no file name",
        ))
    })?;
    Ok(credentials_path.with_file_name(format!(".{}.lock", file_name.to_string_lossy())))
}

fn create_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn credential_lock_is_stale(lock_path: &Path) -> Result<bool> {
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(OakError::Io(e)),
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return Ok(lock_age(lock_path).is_some_and(|age| age > STALE_CREDENTIAL_LOCK_AGE));
    };
    Ok(!is_process_alive(pid))
}

fn lock_age(lock_path: &Path) -> Option<Duration> {
    fs::metadata(lock_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
fn pause_after_credentials_load_for_tests() {
    let pause_ms = SAVE_CREDENTIAL_PAUSE_MS.load(std::sync::atomic::Ordering::SeqCst);
    if pause_ms > 0 {
        thread::sleep(Duration::from_millis(pause_ms));
    }
}

#[cfg(not(test))]
fn pause_after_credentials_load_for_tests() {}

#[cfg(test)]
static SAVE_CREDENTIAL_PAUSE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::{
        choose_author_name, credential_lock_path, load_credentials_from_path, migrated_credential,
        remove_credential_from_path, resolve_token, save_credential_to_path, Credential,
        SAVE_CREDENTIAL_PAUSE_MS,
    };

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

    #[cfg(unix)]
    #[test]
    fn save_credential_to_path_creates_owner_only_credentials_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");

        save_credential_to_path(&path, cred("https://oak.space", "tok-new")).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_credential_to_path_replaces_world_readable_file_with_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        fs::write(&path, "[]").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save_credential_to_path(&path, cred("https://oak.space", "tok-new")).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn remove_credential_to_path_rewrites_atomically_and_cleans_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");

        save_credential_to_path(&path, cred("https://one.example", "tok-one")).unwrap();
        save_credential_to_path(&path, cred("https://two.example", "tok-two")).unwrap();

        assert!(remove_credential_from_path(&path, "https://one.example").unwrap());
        let creds = load_credentials_from_path(&path).unwrap();

        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].server, "https://two.example");
        assert!(
            !credential_lock_path(&path).unwrap().exists(),
            "credentials lock should be removed after save/remove completes"
        );
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "credential writes should not leave temp files behind: {leftovers:?}"
        );
    }

    #[test]
    fn concurrent_save_credential_to_path_preserves_all_entries() {
        struct PauseReset;
        impl Drop for PauseReset {
            fn drop(&mut self) {
                SAVE_CREDENTIAL_PAUSE_MS.store(0, Ordering::SeqCst);
            }
        }

        let _reset = PauseReset;
        SAVE_CREDENTIAL_PAUSE_MS.store(20, Ordering::SeqCst);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        let worker_count = 8;
        let barrier = Arc::new(Barrier::new(worker_count));
        let mut handles = Vec::new();

        for i in 0..worker_count {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                save_credential_to_path(
                    &path,
                    Credential {
                        server: format!("https://server-{i}.example"),
                        token: format!("tok-{i}"),
                        username: format!("user-{i}"),
                    },
                )
                .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut servers: Vec<_> = load_credentials_from_path(&path)
            .unwrap()
            .into_iter()
            .map(|cred| cred.server)
            .collect();
        servers.sort();

        assert_eq!(servers.len(), worker_count);
        for i in 0..worker_count {
            assert!(servers.contains(&format!("https://server-{i}.example")));
        }
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

    #[test]
    fn token_resolver_trims_and_treats_blank_sources_as_unset() {
        assert_eq!(
            resolve_token(
                Some("   ".into()),
                Some(" repo-token \n".into()),
                Some("stored-token".into()),
            ),
            Some("repo-token".into())
        );
        assert_eq!(
            resolve_token(
                Some("\t".into()),
                Some("".into()),
                Some(" stored-token ".into()),
            ),
            Some("stored-token".into())
        );
        assert_eq!(
            resolve_token(Some(" env-token ".into()), Some("repo".into()), None),
            Some("env-token".into())
        );
    }
}
