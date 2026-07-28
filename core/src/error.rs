use thiserror::Error;

#[derive(Debug)]
pub struct FinishPreflightError {
    pub blocker: String,
    pub message: String,
    pub pending_phases: Vec<String>,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
}

impl std::fmt::Display for FinishPreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug)]
pub struct FinishPhaseError {
    pub phase: String,
    pub completed_phases: Vec<String>,
    pub pending_phases: Vec<String>,
    pub message: String,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
}

impl std::fmt::Display for FinishPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug)]
pub struct CommitPhaseError {
    pub phase: String,
    pub committed: bool,
    pub pushed: bool,
    pub published: bool,
    pub remote_contacted: bool,
    pub branch: String,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub unpushed_commit_count: usize,
    pub message: String,
    pub retry_command: Option<String>,
    pub manual_recovery_commands: Vec<String>,
}

impl std::fmt::Display for CommitPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Error, Debug)]
pub enum OakError {
    #[error(
        "Repository not found. Run 'oak init' to create a repository here, or 'oak clone <repo>' to clone an existing one."
    )]
    RepoNotFound,

    #[error("Repository already exists at this location")]
    RepoAlreadyExists,

    #[error("No commits yet")]
    NoCommits,

    #[error("Invalid hash: {0}")]
    InvalidHash(String),

    #[error("{0}")]
    InvalidPath(String),

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

    #[error("{0}")]
    DirtyWorkingTree(String),

    #[error("Conflict detected: remote has diverged. Pull changes first.")]
    ConflictDetected,

    #[error(
        "This branch's local history isn't anchored on the remote's.\nRun 'oak pull' to converge: it re-parents the branch onto the remote and keeps your local commits (overlapping edits open the normal conflict flow).\nIf you want the remote's state instead, 'oak pull --force' re-syncs after parking local commits as '<branch>.orphaned-<timestamp>' — nothing is deleted."
    )]
    LocalCommitsNotInRemoteHistory,

    #[error(
        "The remote branch has commits this clone doesn't have.\nRun 'oak pull' to bring them in and converge (your local commits are kept), then push again."
    )]
    RemoteCommitsNotInLocalHistory,

    #[error("Remote repository not configured")]
    RemoteNotConfigured,

    #[error(
        "remote has moved to {origin} — run `oak push -r {origin}` to update this repo's remote"
    )]
    RemoteMoved { origin: String },

    #[error("Repository '{0}' not found on server")]
    RemoteRepoNotFound(String),

    #[error("Repository '{0}' already exists on server")]
    RemoteRepoAlreadyExists(String),

    #[error("Merge failed: {0}")]
    MergeFailed(String),

    #[error(
        "Incomplete ancestry while finding common ancestor between '{left}' and '{right}'. Missing commit(s): {missing}. Run 'oak fetch' or 'oak pull --force' to repair local history."
    )]
    IncompleteAncestry {
        left: String,
        right: String,
        missing: String,
    },

    #[error(
        "Incomplete manifest data while merging '{left}' with '{right}'. Missing manifest(s): {missing}. Run 'oak fetch' or 'oak pull --force' to repair local history."
    )]
    IncompleteManifestData {
        left: String,
        right: String,
        missing: String,
    },

    #[error(
        "Incomplete commit data while reading {context}. Missing commit(s): {missing}. Run 'oak fetch' or 'oak pull --force' to repair local history."
    )]
    IncompleteCommitData { context: String, missing: String },

    #[error(
        "Incomplete blob data while reading {context}. Missing blob(s): {missing}. Run 'oak fetch' or 'oak pull --force' to repair local history."
    )]
    IncompleteBlobData { context: String, missing: String },

    #[error(
        "No verified common ancestor found between '{left}' and '{right}'. Run 'oak fetch' or 'oak pull --force' to repair local history."
    )]
    NoVerifiedCommonAncestor { left: String, right: String },

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

    #[error("{0}")]
    FinishPreflight(Box<FinishPreflightError>),

    #[error("{0}")]
    FinishPhaseFailed(Box<FinishPhaseError>),

    #[error("{0}")]
    CommitPhaseFailed(Box<CommitPhaseError>),

    #[error(
        "Repository is locked by another process. If no other process is running, remove .oak/wdlock"
    )]
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

    /// The server withheld content because a path-based permission rule
    /// denies this user (the `path-permissions` feature). Not a data gap —
    /// the fix is an access grant, not a re-pull. The message names what was
    /// withheld (paths when known, blob hashes otherwise).
    #[error("{0}")]
    RestrictedContent(String),
}

pub type Result<T> = std::result::Result<T, OakError>;
