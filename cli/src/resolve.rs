use std::fs;
use std::path::{Path, PathBuf};

use oak_core::{GitRepository, Repository, SqliteRepository};
use oak_core::{OakError, Result};

/// Which storage backend `RepoContext` is wired to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Native Oak repo: state lives in `.oak/oak.db`.
    Sqlite,
    /// Git-backed repo: state lives in `<git_dir>` (refs, objects), with Oak-only
    /// sidecar files in `<git_dir>/oak/`.
    Git {
        /// The `.git` directory (or bare repo path).
        git_dir: PathBuf,
    },
}

/// Context for repo operations.
///
/// `oak_dir` is the path reserved for Oak-internal sidecar files (workdir lock,
/// MERGE_HEAD, etc.). For the SQLite backend it's `.oak/`; for the git backend
/// it's `<git_dir>/oak/` and may not exist until a write path needs it.
#[derive(Debug, Clone)]
pub struct RepoContext {
    pub oak_dir: PathBuf,
    pub work_tree: PathBuf,
    pub backend: Backend,
}

impl RepoContext {
    /// Path to the SQLite database. Errors if the backend isn't SQLite — this
    /// guards against commands that bypass `open()` and accidentally create an
    /// empty `oak.db` inside a git-backed repo's sidecar dir.
    pub fn db_path(&self) -> Result<PathBuf> {
        crate::commands::switch::ensure_no_pending(&self.oak_dir)?;
        match &self.backend {
            Backend::Sqlite => Ok(self.oak_dir.join("oak.db")),
            Backend::Git { .. } => Err(OakError::Unsupported {
                backend: "git",
                op: "db_path",
            }),
        }
    }

    /// Open the underlying `Repository` for this context.
    ///
    /// Returns a `Box<dyn Repository>` so callers don't need to branch on backend.
    /// Commands that depend on a feature unique to one backend should check
    /// `repo.capabilities()` and fail with a clear message.
    pub fn open(&self) -> Result<Box<dyn Repository>> {
        crate::commands::switch::ensure_no_pending(&self.oak_dir)?;
        self.open_for_switch()
    }

    pub(crate) fn open_for_switch(&self) -> Result<Box<dyn Repository>> {
        match &self.backend {
            Backend::Sqlite => Ok(Box::new(SqliteRepository::open(
                &self.oak_dir.join("oak.db"),
            )?)),
            Backend::Git { git_dir } => Ok(Box::new(GitRepository::open(git_dir, &self.oak_dir)?)),
        }
    }
}

/// Resolve the repository context from the given path.
///
/// Walks up the directory tree from `path`, returning the first ancestor that
/// contains a repository marker. At each ancestor, resolution prefers a
/// native Oak repo over a git-backed one:
/// 1. `<ancestor>/.oak/` — native Oak repo.
/// 2. `<ancestor>/.git/` — git-backed repo; Oak sidecar dir is `<ancestor>/.git/oak/`.
///
/// This lets users invoke oak commands from any subdirectory of a repo, the
/// same way `git` does. Git-backed resolution returns the sidecar path without
/// creating it, so read-only commands stay filesystem-pure. Returns
/// `RepoNotFound` if no ancestor has either marker.
pub fn resolve(path: &Path) -> Result<RepoContext> {
    // Canonicalize so `..` and symlinks don't confuse the walk. If the exact
    // path doesn't exist, anchor resolution at the nearest existing ancestor
    // rather than walking a raw path chain.
    let start = canonical_resolution_start(path)?;
    // `<home>/.oak` is Oak's *global* state directory (credentials, mounts,
    // version_check, a shared oak.db) — not a repository. If we let the walk
    // treat it as a repo root, running an oak command from a dir that has no
    // repo of its own would open (and migrate) the global oak.db as a repo
    // DB and then scan the entire home tree. Skip it — see
    // [`is_global_state_root`] for why `$HOME` alone is not enough.
    let homes = global_state_homes();
    let mut current: &Path = &start;
    loop {
        let oak_dir = current.join(".oak");
        // A home directory's `.oak/` is skipped and the walk continues (a
        // sibling `.git` below still resolves, as for any directory).
        if oak_dir.is_dir() && !homes.iter().any(|home| home == current) {
            // A `.oak/` holding global-only state is never opened as a repo.
            // Like an incomplete marker, it stops the walk (so a command can
            // never silently land in an outer repository) and only a sibling
            // `.git` can still resolve here.
            if let Some(marker) = global_state_marker(&oak_dir) {
                warn_skipped_global_state(current, &oak_dir, marker);
                if current.join(".git").exists() {
                    let git_dir = current.join(".git");
                    return Ok(RepoContext {
                        oak_dir: git_dir.join("oak"),
                        work_tree: current.to_path_buf(),
                        backend: Backend::Git { git_dir },
                    });
                }
                return Err(OakError::RepoNotFound);
            }
            if oak_dir.join("CLONE_IN_PROGRESS").exists() {
                return Err(OakError::Server(format!(
                    "Oak clone at {} was interrupted before it finished publishing a usable repository. \
                     Remove this directory and re-run `oak clone`.",
                    current.display()
                )));
            }
            // A `.oak/` directory without its database is an empty or partial
            // marker, not a repository. Opening it would *create* `oak.db`,
            // so even read-only commands like `oak status` would mutate the
            // directory and then report a healthy-looking empty repo. Treat it
            // as not-a-repo here (a sibling `.git` still resolves as a
            // git-backed repo below) and never walk past it to a parent repo.
            if !oak_dir.join("oak.db").is_file() {
                if current.join(".git").exists() {
                    let git_dir = current.join(".git");
                    return Ok(RepoContext {
                        oak_dir: git_dir.join("oak"),
                        work_tree: current.to_path_buf(),
                        backend: Backend::Git { git_dir },
                    });
                }
                return Err(OakError::RepoNotFound);
            }
            return Ok(RepoContext {
                oak_dir,
                work_tree: current.to_path_buf(),
                backend: Backend::Sqlite,
            });
        }

        let git_dir = current.join(".git");
        if git_dir.exists() {
            let sidecar = git_dir.join("oak");
            return Ok(RepoContext {
                oak_dir: sidecar,
                work_tree: current.to_path_buf(),
                backend: Backend::Git { git_dir },
            });
        }

        match current.parent() {
            Some(p) if p != current => current = p,
            _ => return Err(OakError::RepoNotFound),
        }
    }
}

/// The nearest ancestor of `path` (inclusive) whose `.oak/` exists but has
/// no `oak.db`, i.e. the incomplete marker that made [`resolve`] stop with
/// `RepoNotFound`. A global state directory (`<home>/.oak`, or one holding
/// global-only state — see [`global_state_marker_root`]) is never reported.
pub fn incomplete_oak_root(path: &Path) -> Option<PathBuf> {
    let start = canonical_resolution_start(path).ok()?;
    let homes = global_state_homes();
    for current in start.ancestors() {
        let oak_dir = current.join(".oak");
        if oak_dir.is_dir() && !homes.iter().any(|home| home == current) {
            if global_state_marker(&oak_dir).is_some() {
                return None;
            }
            return (!oak_dir.join("oak.db").is_file()).then(|| current.to_path_buf());
        }
        if current.join(".git").exists() {
            return None;
        }
    }
    None
}

/// The nearest ancestor of `path` (inclusive) where [`resolve`] stopped
/// because its `.oak/` holds global-only state, with the marker it found.
pub fn global_state_marker_root(path: &Path) -> Option<(PathBuf, &'static str)> {
    let start = canonical_resolution_start(path).ok()?;
    let homes = global_state_homes();
    for current in start.ancestors() {
        let oak_dir = current.join(".oak");
        if oak_dir.is_dir() && !homes.iter().any(|home| home == current) {
            return global_state_marker(&oak_dir).map(|marker| (current.to_path_buf(), marker));
        }
        if current.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Why `dir` must never become a repository root, if it must not: it is a
/// home directory (`None` marker) or its `.oak/` holds the global-only
/// entry `Some(marker)`. Shared by discovery and `oak init`.
pub fn global_state_reason(dir: &Path) -> Option<Option<&'static str>> {
    let dir = fs::canonicalize(dir).ok()?;
    if global_state_homes().contains(&dir) {
        return Some(None);
    }
    global_state_marker(&dir.join(".oak")).map(Some)
}

/// Explain (once per process, on stderr) that `<dir>/.oak` was not treated
/// as a repository because it holds global-only state, and how to recover
/// when it really is a repository that picked up a stray global file.
fn warn_skipped_global_state(dir: &Path, oak_dir: &Path, marker: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut msg = format!(
        "{} is not treated as an Oak repository: its .oak/ contains `{marker}`, which only Oak's per-user global state directory (~/.oak) holds.",
        dir.display()
    );
    if oak_dir.join("oak.db").is_file() {
        msg.push_str(&format!(
            " If this is a repository that picked up a stray global file (e.g. a command run with HOME set to this directory), remove {} and retry; the repository itself is untouched.",
            oak_dir.join(marker).display()
        ));
    }
    crate::output::warning(&msg);
}

/// Files and directories that only Oak's per-user global state directory
/// (`<home>/.oak`) ever contains. Every one is written exclusively under
/// `dirs::home_dir()/.oak` (version check, mount registry, global config,
/// update channel, feedback queue, auto-update log); a repository's `.oak/`
/// never holds them. (`credentials` is deliberately absent: repositories may
/// carry repo-scoped credentials.)
const GLOBAL_STATE_ONLY_ENTRIES: &[&str] = &[
    "version_check",
    "mounts",
    "config.json",
    "update.json",
    "feedback.json",
    "auto-update.log",
];

/// Whether `<dir>/.oak` (`oak_dir`) is a per-user global state directory
/// rather than a repository marker.
///
/// `$HOME` alone is not a sufficient test: when `HOME` is overridden (test
/// fixtures, agents, sandboxes, `sudo -E`, CI) a command run from a non-repo
/// directory under the *real* account home would otherwise walk up to the
/// real `~/.oak` and open its oak.db as a repository database. So a directory
/// is global state if it is any of the home directories in `homes` (`$HOME`
/// and the OS account database's home for the real and effective user), or
/// if its `.oak/` holds an entry only global state ever contains — which also
/// covers another account's home (e.g. `sudo` without `-E`).
#[cfg(test)]
fn is_global_state_root(dir: &Path, oak_dir: &Path, homes: &[PathBuf]) -> bool {
    homes.iter().any(|home| home == dir) || global_state_marker(oak_dir).is_some()
}

/// The first global-only entry present in `oak_dir`, if any.
fn global_state_marker(oak_dir: &Path) -> Option<&'static str> {
    GLOBAL_STATE_ONLY_ENTRIES
        .iter()
        .copied()
        .find(|entry| fs::symlink_metadata(oak_dir.join(entry)).is_ok())
}

/// Canonical home directories whose `.oak/` is global Oak state: `$HOME`
/// (via `dirs::home_dir`) plus, on unix, the account-database home of the
/// real and effective user, which stay correct when `HOME` is overridden.
fn global_state_homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    let candidates = dirs::home_dir().into_iter().chain(account_homes());
    for home in candidates {
        if let Ok(home) = fs::canonicalize(home) {
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
    homes
}

#[cfg(unix)]
fn account_homes() -> Vec<PathBuf> {
    // SAFETY: getuid/geteuid cannot fail and have no preconditions.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    let mut homes = Vec::new();
    for id in [uid, euid] {
        if let Some(home) = account_home(id) {
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
    homes
}

#[cfg(not(unix))]
fn account_homes() -> Vec<PathBuf> {
    // `dirs::home_dir` on Windows queries the known-folder API rather than an
    // overridable environment variable, so it already names the account home.
    Vec::new()
}

/// The home directory recorded for `uid` in the OS account database.
#[cfg(unix)]
fn account_home(uid: libc::uid_t) -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut buf_len = 1024usize;
    loop {
        let mut buf = vec![0 as libc::c_char; buf_len];
        // SAFETY: `passwd` is plain old data; an all-zero value is valid and
        // is only read after getpwuid_r reports success.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the duration of the call and
        // `buf_len` is the true length of `buf`.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf_len, &mut result) };
        if rc == libc::ERANGE && buf_len < (1 << 20) {
            buf_len *= 2;
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` points at a NUL-terminated string
        // inside `buf`, which is still alive.
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        let dir = Path::new(OsStr::from_bytes(dir.to_bytes()));
        return (!dir.as_os_str().is_empty() && dir.is_absolute()).then(|| dir.to_path_buf());
    }
}

fn canonical_resolution_start(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = fs::canonicalize(path) {
        return Ok(path);
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(OakError::Io)?.join(path)
    };

    for ancestor in absolute.ancestors().skip(1) {
        if ancestor.exists() {
            return fs::canonicalize(ancestor).map_err(OakError::Io);
        }
    }

    std::env::current_dir()
        .map_err(OakError::Io)
        .and_then(|cwd| fs::canonicalize(cwd).map_err(OakError::Io))
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_resolution_start, is_global_state_root, resolve, GLOBAL_STATE_ONLY_ENTRIES,
    };
    use oak_core::OakError;

    #[test]
    fn interrupted_clone_marker_refuses_repo_resolution() {
        let temp = tempfile::TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        std::fs::create_dir(&oak_dir).unwrap();
        std::fs::write(oak_dir.join("CLONE_IN_PROGRESS"), "still cloning\n").unwrap();

        let err = resolve(temp.path()).unwrap_err();

        assert!(
            err.to_string().contains("clone")
                && err.to_string().contains("interrupted")
                && err.to_string().contains("re-run `oak clone`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn regular_oak_file_is_not_a_repo_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join(".oak"), "not a repository\n").unwrap();

        let err = resolve(temp.path()).unwrap_err();

        assert!(
            matches!(err, OakError::RepoNotFound),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_oak_dir_is_not_a_repo_and_is_not_mutated() {
        let temp = tempfile::TempDir::new().unwrap();
        let oak_dir = temp.path().join(".oak");
        std::fs::create_dir(&oak_dir).unwrap();

        let err = resolve(temp.path()).unwrap_err();

        assert!(
            matches!(err, OakError::RepoNotFound),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read_dir(&oak_dir).unwrap().count(), 0);
    }

    #[test]
    fn empty_oak_dir_beside_git_resolves_git_backend() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join(".oak")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();

        let ctx = resolve(temp.path()).unwrap();

        assert!(matches!(ctx.backend, super::Backend::Git { .. }));
    }

    #[test]
    fn global_state_dir_is_skipped_by_marker_and_by_home() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let oak_dir = root.join(".oak");
        std::fs::create_dir(&oak_dir).unwrap();
        std::fs::write(oak_dir.join("oak.db"), b"x").unwrap();

        // An account home is global state even with no marker entries.
        assert!(is_global_state_root(
            &root,
            &oak_dir,
            std::slice::from_ref(&root)
        ));
        assert!(!is_global_state_root(&root, &oak_dir, &[]));
        for entry in GLOBAL_STATE_ONLY_ENTRIES {
            let path = oak_dir.join(entry);
            std::fs::write(&path, b"{}").unwrap();
            assert!(is_global_state_root(&root, &oak_dir, &[]), "{entry}");
            std::fs::remove_file(&path).unwrap();
        }
        // Repo-scoped credentials do not make a repository global state.
        std::fs::write(oak_dir.join("credentials"), b"{}").unwrap();
        assert!(!is_global_state_root(&root, &oak_dir, &[]));
    }

    #[cfg(unix)]
    #[test]
    fn account_home_comes_from_the_os_account_database() {
        use super::{account_home, global_state_homes};

        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        // Containers may run as a uid without a passwd entry; when there is
        // one it must be an absolute path and must be in the skip set.
        if let Some(home) = account_home(uid) {
            assert!(home.is_absolute());
            if let Ok(home) = std::fs::canonicalize(home) {
                assert!(global_state_homes().contains(&home));
            }
        }
    }

    #[test]
    fn nonexistent_path_resolution_starts_at_canonical_existing_ancestor() {
        let temp = tempfile::TempDir::new().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let start = canonical_resolution_start(&real.join("missing/child")).unwrap();

        assert_eq!(start, std::fs::canonicalize(&real).unwrap());
    }
}
