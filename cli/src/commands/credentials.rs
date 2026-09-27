use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oak_core::{MetadataKey, OakError, Repository, Result};

/// Resolve the account-wide credential for `remote`: `OAK_API_KEY` (an
/// explicit instruction from the caller), then the login stored for the
/// remote's origin. Blank/whitespace values are unset.
///
/// This never consults a repository key. Commands that operate on a checkout
/// use [`effective_token_for_repository`] (or [`effective_token_with`] with a
/// [`RepositoryCredential`]) so the checkout's key is only presented to the
/// origin that issued it.
pub fn effective_token(remote: &str) -> Option<String> {
    effective_token_with(remote, None)
}

/// Resolve the credential for a command operating on an already-open local
/// repository. This is the canonical repository-aware seam: process override,
/// then the repository key **only if it is bound to `remote`'s origin**, then
/// the account credential for the remote.
pub fn effective_token_for_repository(remote: &str, repo: &dyn Repository) -> Option<String> {
    effective_token_with(remote, repository_credential(repo).as_ref())
}

/// Shared precedence with an already-loaded repository credential. The
/// repository key participates only when its bound origin equals the
/// normalized origin of `remote` (scheme + host + port); for any other
/// destination it is skipped, exactly as if the checkout had no key.
pub fn effective_token_with(
    remote: &str,
    repository: Option<&RepositoryCredential>,
) -> Option<String> {
    resolve_token(
        std::env::var("OAK_API_KEY").ok(),
        repository.and_then(|credential| credential.token_for(remote)),
        get_token_for_server(remote),
    )
}

/// Resolve a repository-aware credential when a command starts from a working
/// path. Outside a repository this is the account-wide [`effective_token`].
pub fn effective_token_for_worktree(remote: &str, work_path: &Path) -> Option<String> {
    let repository = crate::resolve::resolve(work_path)
        .ok()
        .and_then(|context| context.open().ok());
    match repository {
        Some(repository) => effective_token_for_repository(remote, repository.as_ref()),
        None => effective_token(remote),
    }
}

/// A checkout's stored API key together with the origin it is valid for.
/// Constructed only by [`repository_credential`]; the raw token is reachable
/// only through [`RepositoryCredential::token_for`], which enforces the
/// origin binding.
#[derive(Clone)]
pub struct RepositoryCredential {
    origin: String,
    token: String,
}

impl std::fmt::Debug for RepositoryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepositoryCredential")
            .field("origin", &self.origin)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl RepositoryCredential {
    /// Bind `token` to `remote`'s origin. `None` for an invalid remote or a
    /// blank token.
    pub fn bound(remote: &str, token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        Some(Self {
            origin: remote_origin(remote)?,
            token: token.to_string(),
        })
    }

    /// The key, if and only if `remote` is the origin it was issued for.
    pub fn token_for(&self, remote: &str) -> Option<String> {
        (remote_origin(remote).as_deref() == Some(self.origin.as_str())).then(|| self.token.clone())
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// Load the repository key and its bound origin. The binding is
/// `ApiKeyOrigin` when recorded, else (checkouts written by older clients)
/// the stored `RemoteUrl`. A key with no parseable binding is never used.
pub fn repository_credential(repo: &dyn Repository) -> Option<RepositoryCredential> {
    let token = repo.get_metadata(MetadataKey::ApiKey).ok().flatten()?;
    let bound = match repo.get_metadata(MetadataKey::ApiKeyOrigin).ok().flatten() {
        Some(origin) => origin,
        None => repo.get_metadata(MetadataKey::RemoteUrl).ok().flatten()?,
    };
    RepositoryCredential::bound(&bound, &token)
}

/// Persist a repository key issued by `remote`, bound to its origin.
pub fn store_repository_key(repo: &dyn Repository, remote: &str, token: &str) -> Result<()> {
    let origin = remote_origin(remote).ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "cannot bind a repository key to invalid remote {remote:?}"
        ))
    })?;
    repo.set_metadata(MetadataKey::ApiKeyOrigin, &origin)?;
    repo.set_metadata(MetadataKey::ApiKey, token)
}

/// The single writer for a checkout's `RemoteUrl`. Before the stored remote
/// changes, a legacy repository key (no `ApiKeyOrigin`) is pinned to the
/// origin it has been used with so far, so retargeting never silently
/// re-binds the key to the new server.
pub fn set_repository_remote(repo: &dyn Repository, remote: &str) -> Result<()> {
    pin_legacy_repository_key(repo)?;
    repo.set_metadata(MetadataKey::RemoteUrl, remote)
}

/// Follow a *trusted* host move (the caller has already checked
/// `crate::http::is_trusted_origin(new_remote)`): the key issued by the old
/// origin moves with the remote, mirroring [`migrate_server_credential`].
/// A key bound to some third origin is left untouched.
pub fn rebind_repository_key_for_trusted_move(
    repo: &dyn Repository,
    old_remote: &str,
    new_remote: &str,
) -> Result<()> {
    pin_legacy_repository_key(repo)?;
    let (Some(old), Some(new)) = (remote_origin(old_remote), remote_origin(new_remote)) else {
        return Ok(());
    };
    if repo.get_metadata(MetadataKey::ApiKeyOrigin)?.as_deref() == Some(old.as_str()) {
        repo.set_metadata(MetadataKey::ApiKeyOrigin, &new)?;
    }
    Ok(())
}

/// `ApiKeyOrigin` value for a legacy key whose stored remote is missing or
/// unparseable. It is not an origin, so the key it guards is never presented
/// to any server. Recovery: `oak login -r <remote>` (the account login is
/// used instead) or a fresh `oak clone`, which stores a correctly bound key.
pub const UNBOUND_REPOSITORY_KEY_ORIGIN: &str = "unbound:";

/// Pin a legacy key (no `ApiKeyOrigin`) to the origin of the stored remote
/// *before* that remote is rewritten. With no parseable stored remote the
/// key is pinned to [`UNBOUND_REPOSITORY_KEY_ORIGIN`] — never left unpinned,
/// or the next `RemoteUrl` write would become its binding. The write is
/// insert-if-absent, so a concurrent retarget that pinned first wins and is
/// never overwritten with its own new remote.
fn pin_legacy_repository_key(repo: &dyn Repository) -> Result<()> {
    if repo.get_metadata(MetadataKey::ApiKeyOrigin)?.is_some()
        || repo.get_metadata(MetadataKey::ApiKey)?.is_none()
    {
        return Ok(());
    }
    let origin = repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .as_deref()
        .and_then(remote_origin)
        .unwrap_or_else(|| UNBOUND_REPOSITORY_KEY_ORIGIN.to_string());
    repo.insert_metadata_if_absent(MetadataKey::ApiKeyOrigin, &origin)?;
    Ok(())
}

/// Normalized credential origin of a remote URL: lowercase `scheme://host`
/// plus `:port` whenever the port is not the scheme default. Paths,
/// userinfo, queries and fragments are not part of the security boundary.
/// Only `http`/`https` remotes have an origin.
pub fn remote_origin(remote: &str) -> Option<String> {
    let url = reqwest::Url::parse(remote.trim()).ok()?;
    let scheme = url.scheme().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

/// Whether two remote URLs share a credential origin.
pub fn same_remote_origin(a: &str, b: &str) -> bool {
    match (remote_origin(a), remote_origin(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
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

        let deadline = Instant::now() + CREDENTIAL_LOCK_TIMEOUT;
        let mut backoff = crate::workdir_lock::LockBackoff::new();
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
                    // Shared with the working-directory lock: only an owner
                    // proven dead (ESRCH) is reaped, under a reaper mutex so
                    // concurrent reapers cannot both acquire; unknown
                    // liveness waits.
                    if crate::workdir_lock::reap_if_owner_dead(&lock_path)? {
                        continue;
                    }
                    if !backoff.sleep_before(deadline) {
                        return Err(OakError::Io(io::Error::new(
                            ErrorKind::TimedOut,
                            format!(
                                "credentials file is locked by another process: lock {} ({}); if no other oak process is using it, remove that file",
                                lock_path.display(),
                                crate::workdir_lock::describe_lock_owner(&lock_path)
                            ),
                        )));
                    }
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

#[cfg(test)]
fn pause_after_credentials_load_for_tests() {
    let pause_ms = SAVE_CREDENTIAL_PAUSE_MS.load(std::sync::atomic::Ordering::SeqCst);
    if pause_ms > 0 {
        std::thread::sleep(Duration::from_millis(pause_ms));
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
    credential_for_server(&creds, server).map(|c| c.token.clone())
}

/// The stored login for `server`: an exact (trailing-slash-insensitive)
/// match first, else the entry whose normalized origin equals `server`'s.
/// Never a credential for a different origin.
fn credential_for_server<'a>(creds: &'a [Credential], server: &str) -> Option<&'a Credential> {
    let normalized = server.trim_end_matches('/');
    creds
        .iter()
        .find(|c| c.server.trim_end_matches('/') == normalized)
        .or_else(|| creds.iter().find(|c| same_remote_origin(&c.server, server)))
}

/// Get the logged-in username for a given server URL
pub fn get_username_for_server(server: &str) -> Option<String> {
    let creds = load_credentials().ok()?;
    credential_for_server(&creds, server).map(|c| c.username.clone())
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
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        choose_author_name, credential_lock_path, load_credentials_from_path, migrated_credential,
        remove_credential_from_path, resolve_token, save_credential_to_path, Credential,
        SAVE_CREDENTIAL_PAUSE_MS,
    };

    mod origin_binding {
        use oak_core::{MetadataKey, Repository, SqliteRepository};

        use super::super::{
            credential_for_server, rebind_repository_key_for_trusted_move, remote_origin,
            repository_credential, set_repository_remote, store_repository_key, Credential,
        };

        pub(super) fn fresh() -> (tempfile::TempDir, SqliteRepository) {
            repo()
        }

        fn repo() -> (tempfile::TempDir, SqliteRepository) {
            let dir = tempfile::tempdir().unwrap();
            let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
            (dir, repo)
        }

        #[test]
        fn origin_is_scheme_host_port_only() {
            assert_eq!(
                remote_origin("HTTPS://Oak.Space/").as_deref(),
                Some("https://oak.space")
            );
            assert_eq!(
                remote_origin("https://oak.space:443/api?q=1#f").as_deref(),
                Some("https://oak.space")
            );
            assert_eq!(
                remote_origin("https://user:pw@oak.space:8443/x").as_deref(),
                Some("https://oak.space:8443")
            );
            assert_ne!(
                remote_origin("http://oak.space"),
                remote_origin("https://oak.space")
            );
            assert_ne!(
                remote_origin("http://127.0.0.1:1"),
                remote_origin("http://127.0.0.1:2")
            );
            assert_eq!(remote_origin("file:///tmp/x"), None);
            assert_eq!(remote_origin("not a url"), None);
        }

        #[test]
        fn bound_key_is_only_released_to_its_origin() {
            let (_dir, repo) = repo();
            store_repository_key(&repo, "https://a.example/", "k").unwrap();
            let credential = repository_credential(&repo).unwrap();
            assert_eq!(
                credential.token_for("https://A.example").as_deref(),
                Some("k")
            );
            assert_eq!(
                credential.token_for("https://a.example:443/x").as_deref(),
                Some("k")
            );
            assert_eq!(credential.token_for("https://b.example"), None);
            assert_eq!(credential.token_for("http://a.example"), None);
            assert_eq!(credential.token_for("https://a.example:8443"), None);
            assert_eq!(credential.token_for("https://a.example.evil.test"), None);
            let (_dir2, other) = super::super::tests::origin_binding::fresh();
            store_repository_key(&other, "https://a.example", "secret-key-value").unwrap();
            let debug = format!("{:?}", repository_credential(&other).unwrap());
            assert!(!debug.contains("secret-key-value"), "{debug}");
        }

        #[test]
        fn legacy_key_binds_to_stored_remote_and_is_pinned_before_retarget() {
            let (_dir, repo) = repo();
            repo.set_metadata(MetadataKey::RemoteUrl, "https://a.example")
                .unwrap();
            repo.set_metadata(MetadataKey::ApiKey, "legacy").unwrap();
            assert_eq!(
                repository_credential(&repo)
                    .unwrap()
                    .token_for("https://a.example")
                    .as_deref(),
                Some("legacy")
            );
            set_repository_remote(&repo, "https://b.example").unwrap();
            assert_eq!(
                repo.get_metadata(MetadataKey::ApiKeyOrigin)
                    .unwrap()
                    .as_deref(),
                Some("https://a.example")
            );
            let credential = repository_credential(&repo).unwrap();
            assert_eq!(credential.token_for("https://b.example"), None);
            assert_eq!(
                credential.token_for("https://a.example").as_deref(),
                Some("legacy")
            );
        }

        #[test]
        fn legacy_key_without_parseable_remote_is_pinned_unbound_before_retarget() {
            for stored in [None, Some("oak.space"), Some("not a url")] {
                let (_dir, repo) = repo();
                if let Some(stored) = stored {
                    repo.set_metadata(MetadataKey::RemoteUrl, stored).unwrap();
                }
                repo.set_metadata(MetadataKey::ApiKey, "legacy").unwrap();
                set_repository_remote(&repo, "https://b.example").unwrap();
                assert_eq!(
                    repo.get_metadata(MetadataKey::ApiKeyOrigin)
                        .unwrap()
                        .as_deref(),
                    Some(super::super::UNBOUND_REPOSITORY_KEY_ORIGIN),
                    "{stored:?}"
                );
                assert!(repository_credential(&repo).is_none(), "{stored:?}");
                set_repository_remote(&repo, "https://c.example").unwrap();
                assert!(repository_credential(&repo).is_none(), "{stored:?}");
            }
        }

        #[test]
        fn pin_never_overwrites_an_existing_binding() {
            let (_dir, repo) = repo();
            repo.set_metadata(MetadataKey::RemoteUrl, "https://x.example")
                .unwrap();
            repo.set_metadata(MetadataKey::ApiKey, "k").unwrap();
            // A concurrent retarget pinned first.
            assert!(repo
                .insert_metadata_if_absent(MetadataKey::ApiKeyOrigin, "https://a.example")
                .unwrap());
            assert!(!repo
                .insert_metadata_if_absent(MetadataKey::ApiKeyOrigin, "https://x.example")
                .unwrap());
            set_repository_remote(&repo, "https://y.example").unwrap();
            assert_eq!(
                repo.get_metadata(MetadataKey::ApiKeyOrigin)
                    .unwrap()
                    .as_deref(),
                Some("https://a.example")
            );
        }

        #[test]
        fn key_without_any_binding_is_never_used() {
            let (_dir, repo) = repo();
            repo.set_metadata(MetadataKey::ApiKey, "orphan").unwrap();
            assert!(repository_credential(&repo).is_none());
        }

        #[test]
        fn trusted_move_carries_only_a_key_bound_to_the_old_origin() {
            let (_dir, repo) = repo();
            repo.set_metadata(MetadataKey::RemoteUrl, "https://old.example")
                .unwrap();
            repo.set_metadata(MetadataKey::ApiKey, "k").unwrap();
            rebind_repository_key_for_trusted_move(
                &repo,
                "https://old.example",
                "https://oak.space",
            )
            .unwrap();
            assert_eq!(
                repository_credential(&repo)
                    .unwrap()
                    .token_for("https://oak.space")
                    .as_deref(),
                Some("k")
            );

            let (_dir, repo) = repo_bound_elsewhere();
            rebind_repository_key_for_trusted_move(
                &repo,
                "https://old.example",
                "https://oak.space",
            )
            .unwrap();
            assert_eq!(
                repository_credential(&repo)
                    .unwrap()
                    .token_for("https://oak.space"),
                None
            );
        }

        fn repo_bound_elsewhere() -> (tempfile::TempDir, SqliteRepository) {
            let (dir, repo) = repo();
            store_repository_key(&repo, "https://third.example", "k").unwrap();
            repo.set_metadata(MetadataKey::RemoteUrl, "https://old.example")
                .unwrap();
            (dir, repo)
        }

        #[test]
        fn stored_login_lookup_never_crosses_origins() {
            let creds = vec![
                Credential {
                    server: "https://a.example/".to_string(),
                    token: "a".to_string(),
                    username: "u".to_string(),
                },
                Credential {
                    server: "http://127.0.0.1:9000".to_string(),
                    token: "local".to_string(),
                    username: "u".to_string(),
                },
            ];
            let token =
                |server: &str| credential_for_server(&creds, server).map(|c| c.token.clone());
            assert_eq!(token("https://a.example").as_deref(), Some("a"));
            assert_eq!(token("HTTPS://A.EXAMPLE:443").as_deref(), Some("a"));
            assert_eq!(token("http://a.example"), None);
            assert_eq!(token("https://b.example"), None);
            assert_eq!(token("http://127.0.0.1:9000/").as_deref(), Some("local"));
            assert_eq!(token("http://127.0.0.1:9001"), None);
        }
    }

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

    const LOCK_CHILD_ENV: &str = "OAK_CREDENTIALS_LOCK_CHILD";
    const LOCK_PATH_ENV: &str = "OAK_CREDENTIALS_LOCK_PATH";
    const LOCK_INDEX_ENV: &str = "OAK_CREDENTIALS_LOCK_INDEX";
    const LOCK_START_ENV: &str = "OAK_CREDENTIALS_LOCK_START";

    /// Run this test binary as a credential-saving child. `PATH` is blanked so
    /// that nothing the lock does can depend on spawning a helper program: a
    /// helper that cannot be spawned (as happens under process/fd exhaustion)
    /// must never be read as "the lock owner is dead".
    fn spawn_saving_child(test: &str, path: &Path, index: usize, start: &Path) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("PATH", "")
            .env(LOCK_CHILD_ENV, "1")
            .env(LOCK_PATH_ENV, path)
            .env(LOCK_INDEX_ENV, index.to_string())
            .env(LOCK_START_ENV, start)
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Child body: wait for the start file, then save one distinct entry.
    /// Returns true when running as a child (the caller must return).
    fn run_saving_child_if_requested() -> bool {
        if std::env::var_os(LOCK_CHILD_ENV).is_none() {
            return false;
        }
        let path = PathBuf::from(std::env::var_os(LOCK_PATH_ENV).unwrap());
        let index: usize = std::env::var(LOCK_INDEX_ENV).unwrap().parse().unwrap();
        let start = PathBuf::from(std::env::var_os(LOCK_START_ENV).unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !start.exists() {
            assert!(Instant::now() < deadline, "timed out waiting for start");
            thread::sleep(Duration::from_millis(1));
        }
        // Widen the read-modify-write window so a stolen lock loses an entry.
        SAVE_CREDENTIAL_PAUSE_MS.store(10, Ordering::SeqCst);
        save_credential_to_path(
            &path,
            Credential {
                server: format!("https://server-{index}.example"),
                token: format!("tok-{index}"),
                username: format!("user-{index}"),
            },
        )
        .unwrap();
        true
    }

    #[test]
    fn live_lock_owner_is_not_displaced_when_liveness_probe_cannot_run() {
        if run_saving_child_if_requested() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        let lock_path = credential_lock_path(&path).unwrap();
        let start = tmp.path().join("start");
        fs::write(&start, b"go").unwrap();
        // This (live) test process owns the lock.
        fs::write(&lock_path, format!("{}\n", std::process::id())).unwrap();

        let mut child = spawn_saving_child(
            "commands::credentials::tests::live_lock_owner_is_not_displaced_when_liveness_probe_cannot_run",
            &path,
            0,
            &start,
        );
        thread::sleep(Duration::from_millis(750));
        let early_exit = child.try_wait().unwrap();
        let lock_owner = fs::read_to_string(&lock_path).ok();
        let saved_while_locked = path.exists();
        // Release the lock so the child can finish either way.
        let _ = fs::remove_file(&lock_path);
        let status = child.wait().unwrap();

        assert!(
            early_exit.is_none() && !saved_while_locked,
            "a writer displaced a live lock owner (child exited early: {early_exit:?}, \
             credentials written while locked: {saved_while_locked})"
        );
        assert_eq!(
            lock_owner.as_deref().map(str::trim),
            Some(std::process::id().to_string().as_str()),
            "the live owner's lock file must be left in place"
        );
        assert!(status.success(), "child failed after the lock was released");
        let servers: Vec<_> = load_credentials_from_path(&path)
            .unwrap()
            .into_iter()
            .map(|cred| cred.server)
            .collect();
        assert_eq!(servers, vec!["https://server-0.example".to_string()]);
    }

    #[test]
    fn concurrent_processes_saving_credentials_preserve_all_entries() {
        if run_saving_child_if_requested() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        let start = tmp.path().join("start");
        let child_count = 16;
        let children: Vec<_> = (0..child_count)
            .map(|i| {
                spawn_saving_child(
                    "commands::credentials::tests::concurrent_processes_saving_credentials_preserve_all_entries",
                    &path,
                    i,
                    &start,
                )
            })
            .collect();
        fs::write(&start, b"go").unwrap();
        for mut child in children {
            assert!(child.wait().unwrap().success(), "saving child failed");
        }

        let mut servers: Vec<_> = load_credentials_from_path(&path)
            .unwrap()
            .into_iter()
            .map(|cred| cred.server)
            .collect();
        servers.sort();
        let mut expected: Vec<_> = (0..child_count)
            .map(|i| format!("https://server-{i}.example"))
            .collect();
        expected.sort();
        assert_eq!(servers, expected, "a concurrent save lost an entry");
        assert!(
            !credential_lock_path(&path).unwrap().exists(),
            "credentials lock should be released"
        );
    }

    /// Adopted from independent QA (QA-L8 F1, credstale.sh): 16 processes
    /// racing to reap the same dead-owner lock and save distinct entries
    /// must preserve every entry. `OAK_TEST_CREDSTALE_ROUNDS` raises the
    /// round count for stress runs.
    #[test]
    fn concurrent_processes_reaping_a_dead_owner_lock_preserve_all_entries() {
        if run_saving_child_if_requested() {
            return;
        }
        let rounds: usize = std::env::var("OAK_TEST_CREDSTALE_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let child_count = 16;
        let mut lost_rounds = Vec::new();
        for round in 0..rounds {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("credentials");
            let start = tmp.path().join("start");
            let mut exited = Command::new(std::env::current_exe().unwrap())
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let dead_pid = exited.id();
            assert!(exited.wait().unwrap().success());
            fs::write(
                credential_lock_path(&path).unwrap(),
                format!("{dead_pid}\n"),
            )
            .unwrap();

            let children: Vec<_> = (0..child_count)
                .map(|i| {
                    spawn_saving_child(
                        "commands::credentials::tests::concurrent_processes_reaping_a_dead_owner_lock_preserve_all_entries",
                        &path,
                        i,
                        &start,
                    )
                })
                .collect();
            // Let every child reach the start barrier so they race the reap.
            thread::sleep(Duration::from_millis(300));
            fs::write(&start, b"go").unwrap();
            for mut child in children {
                assert!(child.wait().unwrap().success(), "saving child failed");
            }
            let saved = load_credentials_from_path(&path).unwrap().len();
            if saved != child_count {
                lost_rounds.push((round, saved));
            }
        }
        assert!(
            lost_rounds.is_empty(),
            "rounds that lost entries (round, saved of {child_count}): {lost_rounds:?}"
        );
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
