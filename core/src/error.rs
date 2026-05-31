use thiserror::Error;

#[derive(Error, Debug)]
pub enum OakError {
    #[error("Repository not found. Run 'oak init' to create a repository here, or 'oak clone <repo>' to clone an existing one.")]
    RepoNotFound,

    #[error("Repository already exists at this location")]
    RepoAlreadyExists,

    #[error("No commits yet")]
    NoCommits,

    #[error("Invalid hash: {0}")]
    InvalidHash(String),

    #[error("Blob not found: {0}")]
    BlobNotFound(String),

    #[error("Commit not found: {0}")]
    CommitNotFound(String),

    #[error("Branch not found: {0}")]
    BranchNotFound(String),

    #[error("Branch '{0}' already exists")]
    BranchAlreadyExists(String),

    #[error("Branch '{0}' is closed")]
    BranchClosed(String),

    #[error("No commits in branch")]
    NoEdits,

    #[error("Manifest not found: {0}")]
    ManifestNotFound(String),

    #[error("Working directory has uncommitted changes")]
    UncommittedChanges,

    #[error("Conflict detected: remote has diverged. Pull changes first.")]
    ConflictDetected,

    #[error("Remote repository not configured")]
    RemoteNotConfigured,

    #[error("Repository '{0}' not found on server")]
    RemoteRepoNotFound(String),

    #[error("Repository '{0}' already exists on server")]
    RemoteRepoAlreadyExists(String),

    #[error("Merge failed: {0}")]
    MergeFailed(String),

    #[error("Merge conflict: {0} file(s) modified on both branches since fork")]
    MergeConflict(usize),

    #[error("A merge is already in progress. Use 'oak merge --continue' or 'oak merge --abort'.")]
    MergeInProgress,

    #[error("No merge in progress.")]
    NoMergeInProgress,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Server error: {0}")]
    Server(String),

    #[error("R2 storage error: {0}")]
    R2(String),

    #[error("Git operation error: {0}")]
    Git(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Repository is locked by another process. If no other process is running, remove .oak/wdlock")]
    RepoLocked,

    #[error("Operation '{op}' is not supported by the {backend} backend")]
    Unsupported {
        backend: &'static str,
        op: &'static str,
    },

    #[error("Project '{0}' not found in this repository")]
    ProjectNotFound(String),

    #[error("Team '{0}' not found in this organization")]
    TeamNotFound(String),

    #[error("{0}")]
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, OakError>;
