use std::fs;
use std::path::{Path, PathBuf};

use oak_core::{OakError, Result};
use oak_core::{Repository, SqliteRepository};

/// Context for repo operations.
///
/// `oak_dir` is the repository's `.oak/` directory: the SQLite database plus
/// Oak-internal sidecar files (workdir lock, MERGE_HEAD, etc.).
#[derive(Debug, Clone)]
pub struct RepoContext {
    pub oak_dir: PathBuf,
    pub work_tree: PathBuf,
}

impl RepoContext {
    /// Path to the SQLite database.
    pub fn db_path(&self) -> Result<PathBuf> {
        crate::commands::switch::ensure_no_pending(&self.oak_dir)?;
        Ok(self.oak_dir.join("oak.db"))
    }

    /// Open the underlying `Repository` for this context.
    pub fn open(&self) -> Result<Box<dyn Repository>> {
        crate::commands::switch::ensure_no_pending(&self.oak_dir)?;
        self.open_for_switch()
    }

    pub(crate) fn open_for_switch(&self) -> Result<Box<dyn Repository>> {
        Ok(Box::new(SqliteRepository::open(
            &self.oak_dir.join("oak.db"),
        )?))
    }
}

/// Resolve the repository context from the given path.
///
/// Walks up the directory tree from `path`, returning the first ancestor that
/// contains an `.oak/` repository. This lets users invoke oak commands from
/// any subdirectory of a repo, the same way `git` does. A plain git checkout
/// (`.git` with no `.oak/`) stops the walk: it is not an Oak repository, and
/// a command run inside it must not land in an Oak repository further up
/// ([`git_checkout_root`] names it so callers can point at `oak init`, which
/// imports its history). Returns `RepoNotFound` if no repository is found.
pub fn resolve(path: &Path) -> Result<RepoContext> {
    // Canonicalize so `..` and symlinks don't confuse the walk. If the exact
    // path doesn't exist, anchor resolution at the nearest existing ancestor
    // rather than walking a raw path chain.
    let start = canonical_resolution_start(path)?;
    // `<home>/.oak` is Oak's *global* state directory (credentials,
    // version_check, a shared oak.db) — not a repository. If we let the walk
    // treat it as a repo root, running an oak command from a dir that has no
    // repo of its own would open (and migrate) the global oak.db as a repo
    // DB and then scan the entire home tree. Skip it — see
    // [`is_global_state_root`] for why `$HOME` alone is not enough.
    let homes = global_state_homes();
    let mut current: &Path = &start;
    loop {
        let oak_dir = current.join(".oak");
        // A home directory's `.oak/` is skipped and the walk continues.
        if oak_dir.is_dir() && !homes.iter().any(|home| home == current) {
            // A `.oak/` holding global-only state is never opened as a repo.
            // Like an incomplete marker, it stops the walk so a command can
            // never silently land in an outer repository.
            if let Some(marker) = global_state_marker(&oak_dir) {
                warn_skipped_global_state(current, &oak_dir, marker);
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
            // as not-a-repo here and never walk past it to a parent repo.
            if !oak_dir.join("oak.db").is_file() {
                return Err(OakError::RepoNotFound);
            }
            if oak_db_tracked_by_git(current) {
                return Err(OakError::Server(format!(
                    "{} is tracked by git in {}, so it came from the repository, not from \
                     `oak clone` or `oak init`. Oak won't open it: it names the server that \
                     commands send your credentials to. Remove it from the repository \
                     (`git rm -r --cached .oak`) or delete the folder and run `oak init`.",
                    oak_dir.join("oak.db").display(),
                    current.display()
                )));
            }
            return Ok(RepoContext {
                oak_dir,
                work_tree: current.to_path_buf(),
            });
        }

        if current.join(".git").exists() {
            return Err(OakError::RepoNotFound);
        }

        match current.parent() {
            Some(p) if p != current => current = p,
            _ => return Err(OakError::RepoNotFound),
        }
    }
}

/// The plain git checkout that made [`resolve`] stop with `RepoNotFound`:
/// the nearest ancestor of `path` (inclusive) with a `.git` and no `.oak/`
/// repository at or below it. `oak init` there imports its history.
pub fn git_checkout_root(path: &Path) -> Option<PathBuf> {
    let start = canonical_resolution_start(path).ok()?;
    let homes = global_state_homes();
    for current in start.ancestors() {
        if current.join(".oak").is_dir() && !homes.iter().any(|home| home == current) {
            return None;
        }
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
    }
    None
}

/// Whether a sibling git repository tracks `<dir>/.oak/oak.db`. A hostile git
/// repository can commit a whole `.oak/` (database included) that a plain
/// `git clone` checks out; opening it would trust its remote URL, so `oak
/// push` would send the user's key to the repository author's server. Only
/// asks git when `.git` sits beside `.oak/`, which is rare (`oak init` inside
/// a git checkout). When git can't answer, the database is treated as not
/// tracked, so a missing git never locks anyone out of a real Oak repo.
fn oak_db_tracked_by_git(dir: &Path) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    match std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--", ".oak/oak.db"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => false,
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
/// `dirs::home_dir()/.oak` (version check, the mount registry and global config
/// older versions wrote, update channel, feedback queue, auto-update log); a
/// repository's `.oak/` never holds them. (`credentials` is deliberately absent: repositories may
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
        canonical_resolution_start, git_checkout_root, is_global_state_root, resolve,
        GLOBAL_STATE_ONLY_ENTRIES,
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
    fn oak_db_committed_to_git_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::create_dir(root.join(".oak")).unwrap();
        std::fs::write(root.join(".oak").join("oak.db"), b"x").unwrap();

        // Untracked beside git: an ordinary `oak init` inside a git checkout.
        assert!(resolve(root).is_ok());

        // Tracked: it came from the repository.
        git(&["add", "-f", ".oak/oak.db"]);
        let err = resolve(root).unwrap_err();
        assert!(
            err.to_string().contains("tracked by git"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_oak_dir_beside_git_is_not_a_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join(".oak")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();

        let err = resolve(temp.path()).unwrap_err();

        assert!(
            matches!(err, OakError::RepoNotFound),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plain_git_checkout_is_not_a_repo_and_stops_the_walk() {
        let temp = tempfile::TempDir::new().unwrap();
        let outer = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir(outer.join(".oak")).unwrap();
        std::fs::write(outer.join(".oak").join("oak.db"), b"x").unwrap();
        let inner = outer.join("vendor");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        std::fs::create_dir(inner.join("src")).unwrap();

        let err = resolve(&inner.join("src")).unwrap_err();

        assert!(
            matches!(err, OakError::RepoNotFound),
            "unexpected error: {err}"
        );
        assert_eq!(git_checkout_root(&inner.join("src")), Some(inner.clone()));
        assert_eq!(git_checkout_root(&outer), None);
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
