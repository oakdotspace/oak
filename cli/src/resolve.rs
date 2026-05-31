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
/// `oak_dir` is always a writable directory for Oak-internal sidecar files
/// (workdir lock, MERGE_HEAD, etc.). For the SQLite backend it's `.oak/`;
/// for the git backend it's `<git_dir>/oak/`. This keeps callers that read
/// or write to `oak_dir` working unchanged across backends.
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
        match &self.backend {
            Backend::Sqlite => Ok(Box::new(SqliteRepository::open(&self.db_path()?)?)),
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
/// same way `git` does. Returns `RepoNotFound` if no ancestor has either marker.
pub fn resolve(path: &Path) -> Result<RepoContext> {
    // Canonicalize so `..` and symlinks don't confuse the walk. Fall back to
    // the raw path if canonicalization fails (e.g., path doesn't exist on disk
    // — uncommon in practice since `cwd` is the typical caller).
    let start = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // `$HOME/.oak` is Oak's *global* state directory (credentials, mounts,
    // server-data, a shared oak.db) — not a repository. If we let the walk
    // treat it as a repo root, running an oak command from `$HOME` (or any
    // dir that has no repo of its own) would resolve to home and then try to
    // scan the entire home tree, blowing up on the first TCC-protected
    // subdirectory with "Operation not permitted". Skip it explicitly.
    let home = dirs::home_dir().and_then(|h| fs::canonicalize(h).ok());
    let mut current: &Path = &start;
    loop {
        let oak_dir = current.join(".oak");
        if oak_dir.exists() && home.as_deref() != Some(current) {
            return Ok(RepoContext {
                oak_dir,
                work_tree: current.to_path_buf(),
                backend: Backend::Sqlite,
            });
        }

        let git_dir = current.join(".git");
        if git_dir.exists() {
            let sidecar = git_dir.join("oak");
            // Best-effort: most read paths don't need this directory, so don't fail
            // if creation is denied — we'll surface the error if/when we actually try
            // to write to it.
            let _ = fs::create_dir_all(&sidecar);
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
