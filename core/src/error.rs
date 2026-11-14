use thiserror::Error;

#[derive(Debug)]
pub struct FinishPreflightError {
    pub blocker: FinishBlocker,
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
    #[error("Disk full at '{path}': no free storage is available for the repository database or its journal")]
    DiskFull { path: String },

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

    #[error("Working tree changed while capture was in progress: {detail}. Retry the capture.")]
    WorktreeCaptureRaced { detail: String },

    #[error("Change capture does not support repository hash format {0:?}")]
    UnsupportedChangeCaptureFormat(String),

    #[error("Change capture excluded requested path {path:?}: {reason}. {remedy}")]
    ChangeCaptureScopeExcluded {
        path: String,
        reason: String,
        remedy: String,
    },

    #[error("Captured blob integrity check failed for {path:?} ({hash}): {detail}")]
    CapturedBlobIntegrity {
        path: String,
        hash: String,
        detail: String,
    },

    #[error("Captured tree integrity check failed for {hash}: {detail}")]
    CapturedTreeIntegrity { hash: String, detail: String },

    #[error("Change capture {capture_id:?} was not found. Rerun `oak change capture --json` to create a durable local lookup record.")]
    ChangeCaptureNotFound { capture_id: String },

    #[error("Change capture {capture_id:?} failed durable lookup validation: {detail}")]
    ChangeCaptureIntegrity { capture_id: String, detail: String },

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

    /// `next_commands`: the exact next steps the assessment that raised this
    /// derived from its evidence (never `oak merge`).
    #[error("Merge invariant violation: {detail}")]
    MergeInvariantViolation {
        detail: String,
        next_commands: Vec<String>,
    },

    /// `next_commands`: as on [`OakError::MergeInvariantViolation`].
    #[error("Merge prediction uncertified: {detail}")]
    MergePredictionUncertified {
        detail: String,
        next_commands: Vec<String>,
    },

    /// A remote content-integrity proof ran out of budget before it could
    /// conclude. This is neither proof of health nor evidence of missing
    /// content; callers must not treat it as either.
    #[error("Integrity proof inconclusive: {detail}")]
    IntegrityInconclusive {
        detail: String,
        /// Exact, shell-quoted invocations for the natural next step, in
        /// preference order (e.g. the narrower `--shallow` clone first).
        recommended_next_commands: Vec<String>,
    },

    /// The server's clone admission (integrity snapshot) token is too large to
    /// send back in the pull request URL. Raised before the pull request is
    /// sent; nothing was downloaded. The token itself is never echoed.
    #[error("Clone admission token too large: {detail}")]
    CloneAdmissionTokenTooLarge {
        detail: String,
        /// Exact, shell-quoted invocations that work from the caller's
        /// working directory (the same clone with `--shallow`), or empty
        /// when no narrower retry of this invocation exists.
        recommended_next_commands: Vec<String>,
    },

    /// The server refused to start an integrity proof because it was busy
    /// and the client's bounded wait for admission ran out. Nothing was
    /// downloaded or created.
    #[error("Integrity proof not admitted: {0}")]
    IntegrityAdmissionBusy(String),

    #[error("A merge is already in progress. Use 'oak merge --continue' or 'oak merge --abort'.")]
    MergeInProgress,

    /// A working-tree update (pull, sync, reset, ...) was killed part-way and
    /// its record is still in `.oak/WORKTREE_UPDATE`. `next_commands` are the
    /// exact recovery invocations.
    #[error("{message}")]
    InterruptedWorktreeUpdate {
        message: String,
        next_commands: Vec<String>,
    },

    /// A working-tree transition (branch creation, pull, sync, switch, ...)
    /// would have overwritten uncommitted work: a path it writes holds
    /// content that is neither the snapshot it moves from nor the one it
    /// moves to, or something the user owns is in the way of it. Refused
    /// before the first write. `paths` is a bounded sample of `(path, why)`;
    /// `total` counts them all. The message and the next commands are both
    /// derived from `transition` and `in_the_way`, so they always agree.
    #[error(
        "{}",
        local_changes_message(command, clean_command.as_deref(), *transition, paths, *total, *in_the_way)
    )]
    LocalChangesWouldBeOverwritten {
        /// The refused invocation, shell-quoted, credentials redacted.
        command: String,
        /// `command` with `--clean` added, when that is a valid invocation.
        clean_command: Option<String>,
        transition: WorktreeTransition,
        paths: Vec<(String, String)>,
        total: usize,
        /// Some path is in the way in a way a commit does not resolve (an
        /// ignored file, a file/directory obstruction, a name differing only
        /// in case): it has to be moved aside first.
        in_the_way: bool,
    },

    #[error("No merge in progress.")]
    NoMergeInProgress,

    /// `oak pull --continue` / `--abort` with no paused parent sync.
    #[error("Merge failed: No sync in progress.")]
    NoSyncInProgress,

    /// The named branch is the trunk, which exists only on the server and is
    /// never checked out locally.
    #[error("IO error: {0}")]
    TrunkIsServerOnly(String),

    /// No stored credential for `remote`. Nothing was sent.
    #[error("Not logged in to {remote}. Run `oak login` first.")]
    NotLoggedIn { remote: String },

    /// The checkout is not linked to a remote repository yet; linking it
    /// (`oak push --repo ORG/REPO`) is the fix.
    #[error("Configuration error: {0}")]
    RepoNotLinked(String),

    /// An `oak feedback` request failed. Feedback keeps its own exit code
    /// (1) for remote failures; `code` is the closed machine code, and
    /// `retryable` is set only where the caller knows the answer.
    #[error("{message}")]
    FeedbackFailed {
        message: String,
        /// The request was malformed and nothing was sent (exit 2).
        usage: bool,
        code: ErrorCode,
        retryable: Option<bool>,
        next_commands: Vec<String>,
        /// `oak feedback transition` only: what was sent and its outcome.
        transition: Option<serde_json::Value>,
    },

    /// `oak root pull|push` refused before changing anything.
    #[error("{message}")]
    RootRefused {
        reason: RootRefusal,
        message: String,
        /// The paths in conflict (`root_conflict`), else empty.
        paths: Vec<String>,
        next_commands: Vec<String>,
    },

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

    #[error(
        "{operation} outcome is unconfirmed: {reason}. No automatic mutation retry was attempted; inspect remote state read-only before deciding whether to retry."
    )]
    PublicationUnconfirmed {
        operation: String,
        reason: String,
        reconciliation_commands: Vec<String>,
    },

    #[error(
        "The server at {remote} refused the request with HTTP {status}: {}",
        if *status == 403 {
            "the credential is not allowed to do this"
        } else {
            "authentication is required. Run `oak login` (or set OAK_API_KEY) and retry"
        }
    )]
    AuthRequired { remote: String, status: u16 },

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
        "Repository is locked by another process. If no other oak process is running, remove the lock file (.oak/wdlock, or .oak/publication-attempts/wdlock while publishing)"
    )]
    RepoLocked,

    #[error("Operation '{op}' is not supported by the {backend} backend")]
    Unsupported {
        backend: &'static str,
        op: &'static str,
    },

    #[error("Group '{0}' not found in this organization")]
    GroupNotFound(String),

    #[error("{0}")]
    InvalidArgument(String),

    /// The command line itself was rejected by the argument parser (unknown
    /// flag, missing value, conflicting flags). Unlike [`InvalidArgument`],
    /// which is also raised for conditions that can change, this can never
    /// succeed unchanged.
    ///
    /// [`InvalidArgument`]: OakError::InvalidArgument
    #[error("{0}")]
    Usage(String),

    /// `oak commit` on a detached HEAD: there is no branch to record onto.
    #[error("{0}")]
    DetachedHeadCommit(String),

    /// The server withheld content because a path-based permission rule
    /// denies this user (an access boundary on the server). Not a data gap —
    /// the fix is an access grant, not a re-pull. The message names what was
    /// withheld (paths when known, blob hashes otherwise).
    #[error("{0}")]
    RestrictedContent(String),

    /// A read-modify-write of the branch description (`oak desc --append`)
    /// was refused because the local copy is not a verified image of the
    /// remote one. Nothing was written locally or remotely. `state` is a
    /// stable machine label (`pending_local_edit`, `remote_changed`,
    /// `remote_unverified`); `refresh_commands` are exact commands that bring
    /// the local copy up to date or let the caller inspect both sides.
    #[error("{message}")]
    DescriptionStale {
        branch: String,
        state: DescriptionStaleState,
        message: String,
        refresh_commands: Vec<String>,
    },

    /// The approvals gate or `oak approve` refused (`code` says why). Waiting
    /// never clears it.
    #[error("{message}")]
    ApprovalRefused {
        /// The closed code for the refusal reason.
        code: ErrorCode,
        /// The reason exactly as the server sent it, when this client does
        /// not know it (`code` is then [`ErrorCode::ApprovalRefused`]).
        server_code: Option<String>,
        message: String,
        missing: Vec<MissingApproval>,
        /// Other boundaries missing an approval, which the caller can't read.
        hidden: usize,
        recommended_next_commands: Vec<String>,
    },

    /// Publication is impossible with the protocols the server advertises
    /// (for example the push needs `staged_v1` and the server has not
    /// activated it). A deterministic server-capability refusal, not a usage
    /// error: retrying unchanged cannot succeed until the server's
    /// capabilities or the outgoing history change. `message` names the
    /// measured totals and the limits they exceed and says whether remote
    /// state was mutated.
    #[error("{message}")]
    PushCapabilityUnavailable {
        message: String,
        recommended_next_commands: Vec<String>,
    },

    /// A partial checkout (a view) holds work outside its directories: the
    /// server would refuse the push, so it's caught before the commit.
    #[error("{} path(s) outside this checkout's directories (first: {}); add them (`oak sparse add DIR`), or remove them", paths.len(), paths.first().map(String::as_str).unwrap_or_default())]
    PathOutsideView {
        paths: Vec<String>,
        recommended_next_commands: Vec<String>,
    },
}

/// An ownership boundary a merge still needs an approval for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MissingApproval {
    /// The boundary's directory (`""` is the repo root).
    pub path: String,
    /// The team that owns it.
    pub owner_team: String,
    /// Who can approve (principals such as `@team/web`); empty when no one can.
    pub approvers: Vec<String>,
}

pub type Result<T> = std::result::Result<T, OakError>;

// ---------------------------------------------------------------------------
// Machine contract vocabulary
// ---------------------------------------------------------------------------

/// Generates a closed, field-less enum together with its complete `ALL` list
/// and its stable wire string, from one table. Adding an entry here is the
/// only way to add a value, so `ALL` can never fall out of sync.
#[macro_export]
macro_rules! closed_vocabulary {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident => $wire:literal, )+ }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis enum $name { $( $(#[$vmeta])* $variant, )+ }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant, )+ ];

            /// The stable machine string.
            pub const fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $wire, )+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool { self.as_str() == other }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool { self.as_str() == *other }
        }
    };
}

closed_vocabulary! {
    /// The closed set of `error.code` values a `--json` failure can carry.
    ///
    /// Codes are append-only: a string, once shipped, is never renamed or
    /// reused for a different condition. There is deliberately no generic
    /// member (`error`, `unknown`): a new failure needs a new code here, and
    /// the CLI's one exhaustive `OakError` mapping (`oak_cli::contract`)
    /// will not compile until it names one.
    ///
    /// `RemoteError` is the only deliberately coarse member: a failure
    /// reported by, or while talking to, the remote that has not been
    /// classified further. Classified remote conditions (connection refused,
    /// rate limited, not found, ...) get their own entries.
    pub enum ErrorCode {
        // Usage and configuration.
        InvalidArgument => "invalid_argument",
        InvalidHash => "invalid_hash",
        ConfigurationError => "configuration_error",
        Unsupported => "unsupported",
        // Local repository state.
        RepoNotFound => "repo_not_found",
        RepoAlreadyExists => "repo_already_exists",
        RepoLocked => "repo_locked",
        NoCommits => "no_commits",
        NoEdits => "no_edits",
        BranchNotFound => "branch_not_found",
        BranchAlreadyExists => "branch_already_exists",
        BranchClosed => "branch_closed",
        BranchIsServerOnly => "branch_is_server_only",
        CommitNotFound => "commit_not_found",
        BlobNotFound => "blob_not_found",
        ManifestNotFound => "manifest_not_found",
        GroupNotFound => "group_not_found",
        RestrictedContent => "restricted_content",
        // Local machine.
        DiskFull => "disk_full",
        IoError => "io_error",
        DatabaseError => "database_error",
        SerializationError => "serialization_error",
        GitError => "git_error",
        // Working tree.
        DirtyWorktree => "dirty_worktree",
        InterruptedWorktreeUpdate => "interrupted_worktree_update",
        PathOutsideView => "path_outside_view",
        LocalChangesWouldBeOverwritten => "local_changes_would_be_overwritten",
        // Change capture.
        WorktreeCaptureRaced => "worktree_capture_raced",
        CapturedBlobIntegrity => "captured_blob_integrity",
        CapturedTreeIntegrity => "captured_tree_integrity",
        ChangeCaptureNotFound => "change_capture_not_found",
        ChangeCaptureIntegrity => "change_capture_integrity",
        CaptureScopeExcluded => "capture_scope_excluded",
        // History, merges and conflicts.
        ConflictDetected => "conflict_detected",
        MergeInProgress => "merge_in_progress",
        NoMergeInProgress => "no_merge_in_progress",
        NoSyncInProgress => "no_sync_in_progress",
        MergeFailed => "merge_failed",
        MergeInvariantViolation => "merge_invariant_violation",
        MergePredictionUncertified => "merge_prediction_uncertified",
        LocalCommitsNotInRemoteHistory => "local_commits_not_in_remote_history",
        RemoteCommitsNotInLocalHistory => "remote_commits_not_in_local_history",
        IncompleteAncestry => "incomplete_ancestry",
        IncompleteManifestData => "incomplete_manifest_data",
        IncompleteCommitData => "incomplete_commit_data",
        IncompleteBlobData => "incomplete_blob_data",
        NoVerifiedCommonAncestor => "no_verified_common_ancestor",
        // Remote access.
        RemoteError => "remote_error",
        RemoteMoved => "remote_moved",
        RemoteRepoNotFound => "remote_repo_not_found",
        RemoteRepoAlreadyExists => "remote_repo_already_exists",
        AuthRequired => "auth_required",
        AuthDenied => "auth_denied",
        PublicationUnconfirmed => "publication_unconfirmed",
        PushCapabilityUnavailable => "push_capability_unavailable",
        IntegrityInconclusive => "integrity_inconclusive",
        IntegrityAdmissionBusy => "integrity_admission_busy",
        CloneAdmissionTokenTooLarge => "clone_admission_token_too_large",
        // Multi-phase commands.
        FinishPreflightFailed => "finish_preflight_failed",
        FinishPhaseFailed => "finish_phase_failed",
        CommitPhaseFailed => "commit_phase_failed",
        DescriptionStale => "description_stale",
        // Approvals.
        ApprovalsRequired => "approvals_required",
        ApprovalRefused => "approval_refused",
        ReviewKeyRequired => "review_key_required",
        BranchMoved => "branch_moved",
        SelfApproval => "self_approval",
        NotAnOwner => "not_an_owner",
        NothingToApprove => "nothing_to_approve",
        BoundaryNotTouched => "boundary_not_touched",
        BoundaryOutsideView => "boundary_outside_view",
        ServiceAccount => "service_account",
        ApprovalInternal => "internal",
        ApprovalNotFound => "not_found",
        View => "view",
        // `oak root pull|push`.
        RootConflict => "root_conflict",
        PullRequired => "pull_required",
        HeadMoved => "head_moved",
        // `oak feedback transition` (keeps its own exit codes).
        FeedbackTransitionPreflightFailed => "feedback_transition_preflight_failed",
        FeedbackTransitionUnavailable => "feedback_transition_unavailable",
        FeedbackTransitionUnconfirmed => "feedback_transition_unconfirmed",
        FeedbackTransitionRejected => "feedback_transition_rejected",
        FeedbackTransitionOutputFailed => "feedback_transition_output_failed",
        FeedbackCapabilityRequestFailed => "feedback_capability_request_failed",
        FeedbackCapabilityUnauthorized => "feedback_capability_unauthorized",
        FeedbackCapabilityUnavailable => "feedback_capability_unavailable",
        FeedbackCapabilityRedirect => "feedback_capability_redirect",
        FeedbackCapabilityInvalid => "feedback_capability_invalid",
        FeedbackCapabilityUnsupported => "feedback_capability_unsupported",
        RevisionConflict => "revision_conflict",
    }
}

closed_vocabulary! {
    /// Why `oak finish` refused before mutating anything
    /// (`error.finish.blocker`).
    pub enum FinishBlocker {
        InvalidDescription => "invalid_description",
        MergeInProgress => "merge_in_progress",
        SyncInProgress => "sync_in_progress",
        NoCurrentBranch => "no_current_branch",
        BranchNotFound => "branch_not_found",
        BranchClosed => "branch_closed",
        RemoteNotConfigured => "remote_not_configured",
        RemoteIdentityMissing => "remote_identity_missing",
        AuthMissing => "auth_missing",
        AuthFailed => "auth_failed",
        RemoteRepoMissing => "remote_repo_missing",
        RemoteUnreachable => "remote_unreachable",
    }
}

closed_vocabulary! {
    /// Why `oak desc --append` refused (`error.state`).
    pub enum DescriptionStaleState {
        PendingLocalEdit => "pending_local_edit",
        RemoteChanged => "remote_changed",
        RemoteUnverified => "remote_unverified",
        LocalChanged => "local_changed",
    }
}

closed_vocabulary! {
    /// Why `oak root pull|push` refused; the wire string is the error code.
    pub enum RootRefusal {
        Conflict => "root_conflict",
        PullRequired => "pull_required",
        HeadMoved => "head_moved",
    }
}

/// Which kind of working-tree transition refused with
/// [`OakError::LocalChangesWouldBeOverwritten`]: what resolves the refusal
/// depends on it. Set by the command that knows (the planner raises
/// [`WorktreeTransition::Other`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeTransition {
    /// `oak pull` (parent sync), and the watch daemon's pull.
    Pull,
    /// `oak switch -c` / `oak start`: a new branch from the newer main.
    CreateBranch,
    /// Anything else that moves the tree.
    Other,
}

fn local_changes_message(
    command: &str,
    clean_command: Option<&str>,
    transition: WorktreeTransition,
    paths: &[(String, String)],
    total: usize,
    in_the_way: bool,
) -> String {
    let mut message = format!(
        "`{command}` would overwrite or remove local work in {total} path(s), so nothing was \
         changed:"
    );
    for (path, why) in paths {
        message.push_str(&format!("\n  {path} ({why})"));
    }
    if total > paths.len() {
        message.push_str(&format!("\n  ... and {} more", total - paths.len()));
    }
    if in_the_way {
        message.push_str(&format!(
            "\nMove these out of the way (rename, move or delete them; a commit does not \
             resolve this), then run `{command}` again."
        ));
        return message;
    }
    match transition {
        WorktreeTransition::Pull => message.push_str(
            "\nCommit them (`oak commit`) and run `oak pull` again: main is then merged into \
             this branch, and a path changed on both sides is reported as a conflict to resolve \
             instead of being overwritten.",
        ),
        WorktreeTransition::CreateBranch => {
            message.push_str(
                "\nThe new branch starts from the newer main, which has different content at \
                 these paths than your uncommitted version. Either:\n  - keep your changes on \
                 the current branch and merge main into it: `oak commit`, then `oak pull` (a \
                 path changed on both sides is reported as a conflict to resolve); or",
            );
            match clean_command {
                Some(clean) => message.push_str(&format!(
                    "\n  - create the branch without them: `{clean}` (DISCARDS these \
                     uncommitted changes)."
                )),
                None => message.push_str(&format!(
                    "\n  - create the branch without them: run `{command}` with `--clean` \
                     (DISCARDS these uncommitted changes)."
                )),
            }
        }
        WorktreeTransition::Other => message.push_str(&format!(
            "\nCommit them (`oak commit`) or move them aside, then run `{command}` again."
        )),
    }
    message
}

impl ErrorCode {
    /// The code for an approval refusal reason sent by the server
    /// (`oak-web` `api/approvals.rs`) or raised by this client. Every reason
    /// the server emits keeps its exact string; a reason from a newer server
    /// is `None` and is reported as `approval_refused` plus `server_code`.
    pub fn from_approval_reason(reason: &str) -> Option<ErrorCode> {
        Self::APPROVAL_REASONS
            .iter()
            .copied()
            .find(|code| code.as_str() == reason)
    }

    /// Every approval refusal reason this client knows.
    pub const APPROVAL_REASONS: &'static [ErrorCode] = &[
        // Raised by the client.
        ErrorCode::ApprovalsRequired,
        ErrorCode::ApprovalRefused,
        // The server's table.
        ErrorCode::BoundaryNotTouched,
        ErrorCode::BoundaryOutsideView,
        ErrorCode::BranchClosed,
        ErrorCode::BranchMoved,
        ErrorCode::ApprovalInternal,
        ErrorCode::NotAnOwner,
        ErrorCode::ApprovalNotFound,
        ErrorCode::NothingToApprove,
        ErrorCode::ReviewKeyRequired,
        ErrorCode::SelfApproval,
        ErrorCode::ServiceAccount,
        ErrorCode::View,
    ];
}

/// The CLI exit-code table (`oak --help`). Closed: a failure exits with one
/// of these and nothing else; `0` is success and is not a member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExitCode {
    /// 1 — a failure with no more specific class.
    Failure,
    /// 2 — the invocation itself is wrong.
    Usage,
    /// 3 — the repository is locked by another process (retry).
    Locked,
    /// 4 — the working tree blocked the operation.
    WorktreeBlocked,
    /// 5 — a merge/sync conflict or an in-progress conflict state.
    Conflict,
    /// 6 — network, server, or authentication failure.
    Remote,
    /// 7 — merge prediction uncertified.
    MergeUncertified,
    /// 8 — integrity proof inconclusive.
    IntegrityInconclusive,
}

impl ExitCode {
    pub const ALL: &'static [ExitCode] = &[
        ExitCode::Failure,
        ExitCode::Usage,
        ExitCode::Locked,
        ExitCode::WorktreeBlocked,
        ExitCode::Conflict,
        ExitCode::Remote,
        ExitCode::MergeUncertified,
        ExitCode::IntegrityInconclusive,
    ];

    pub const fn code(self) -> i32 {
        match self {
            ExitCode::Failure => 1,
            ExitCode::Usage => 2,
            ExitCode::Locked => 3,
            ExitCode::WorktreeBlocked => 4,
            ExitCode::Conflict => 5,
            ExitCode::Remote => 6,
            ExitCode::MergeUncertified => 7,
            ExitCode::IntegrityInconclusive => 8,
        }
    }
}

closed_vocabulary! {
    /// One member per [`OakError`] variant, so tests can enumerate the
    /// variants and prove a property holds for every one of them.
    /// [`OakError::kind`] is an exhaustive match, so adding an `OakError`
    /// variant without adding its kind here does not compile.
    pub enum OakErrorKind {
        DiskFull => "DiskFull",
        RepoNotFound => "RepoNotFound",
        RepoAlreadyExists => "RepoAlreadyExists",
        NoCommits => "NoCommits",
        InvalidHash => "InvalidHash",
        InvalidPath => "InvalidPath",
        BlobNotFound => "BlobNotFound",
        CommitNotFound => "CommitNotFound",
        BranchNotFound => "BranchNotFound",
        BranchAlreadyExists => "BranchAlreadyExists",
        BranchClosed => "BranchClosed",
        NoEdits => "NoEdits",
        ManifestNotFound => "ManifestNotFound",
        UncommittedChanges => "UncommittedChanges",
        WorktreeCaptureRaced => "WorktreeCaptureRaced",
        UnsupportedChangeCaptureFormat => "UnsupportedChangeCaptureFormat",
        ChangeCaptureScopeExcluded => "ChangeCaptureScopeExcluded",
        CapturedBlobIntegrity => "CapturedBlobIntegrity",
        CapturedTreeIntegrity => "CapturedTreeIntegrity",
        ChangeCaptureNotFound => "ChangeCaptureNotFound",
        ChangeCaptureIntegrity => "ChangeCaptureIntegrity",
        DirtyWorkingTree => "DirtyWorkingTree",
        ConflictDetected => "ConflictDetected",
        LocalCommitsNotInRemoteHistory => "LocalCommitsNotInRemoteHistory",
        RemoteCommitsNotInLocalHistory => "RemoteCommitsNotInLocalHistory",
        RemoteNotConfigured => "RemoteNotConfigured",
        RemoteMoved => "RemoteMoved",
        RemoteRepoNotFound => "RemoteRepoNotFound",
        RemoteRepoAlreadyExists => "RemoteRepoAlreadyExists",
        MergeFailed => "MergeFailed",
        IncompleteAncestry => "IncompleteAncestry",
        IncompleteManifestData => "IncompleteManifestData",
        IncompleteCommitData => "IncompleteCommitData",
        IncompleteBlobData => "IncompleteBlobData",
        NoVerifiedCommonAncestor => "NoVerifiedCommonAncestor",
        MergeConflict => "MergeConflict",
        MergeInvariantViolation => "MergeInvariantViolation",
        MergePredictionUncertified => "MergePredictionUncertified",
        IntegrityInconclusive => "IntegrityInconclusive",
        CloneAdmissionTokenTooLarge => "CloneAdmissionTokenTooLarge",
        IntegrityAdmissionBusy => "IntegrityAdmissionBusy",
        MergeInProgress => "MergeInProgress",
        InterruptedWorktreeUpdate => "InterruptedWorktreeUpdate",
        NoMergeInProgress => "NoMergeInProgress",
        Io => "Io",
        Database => "Database",
        Serialization => "Serialization",
        Http => "Http",
        Server => "Server",
        PublicationUnconfirmed => "PublicationUnconfirmed",
        AuthRequired => "AuthRequired",
        R2 => "R2",
        Git => "Git",
        Config => "Config",
        FinishPreflight => "FinishPreflight",
        FinishPhaseFailed => "FinishPhaseFailed",
        CommitPhaseFailed => "CommitPhaseFailed",
        RepoLocked => "RepoLocked",
        Unsupported => "Unsupported",
        GroupNotFound => "GroupNotFound",
        InvalidArgument => "InvalidArgument",
        Usage => "Usage",
        DetachedHeadCommit => "DetachedHeadCommit",
        NoSyncInProgress => "NoSyncInProgress",
        TrunkIsServerOnly => "TrunkIsServerOnly",
        NotLoggedIn => "NotLoggedIn",
        RepoNotLinked => "RepoNotLinked",
        FeedbackFailed => "FeedbackFailed",
        RootRefused => "RootRefused",
        RestrictedContent => "RestrictedContent",
        DescriptionStale => "DescriptionStale",
        ApprovalRefused => "ApprovalRefused",
        PushCapabilityUnavailable => "PushCapabilityUnavailable",
        PathOutsideView => "PathOutsideView",
        LocalChangesWouldBeOverwritten => "LocalChangesWouldBeOverwritten",
    }
}

impl OakError {
    /// Name the transition a [`OakError::LocalChangesWouldBeOverwritten`]
    /// refused, so its message and next commands fit it. Any other error is
    /// returned unchanged.
    pub fn in_transition(mut self, kind: WorktreeTransition) -> Self {
        if let OakError::LocalChangesWouldBeOverwritten { transition, .. } = &mut self {
            *transition = kind;
        }
        self
    }

    /// Which variant this is. Exhaustive on purpose (no wildcard arm).
    pub fn kind(&self) -> OakErrorKind {
        match self {
            OakError::DiskFull { .. } => OakErrorKind::DiskFull,
            OakError::RepoNotFound => OakErrorKind::RepoNotFound,
            OakError::RepoAlreadyExists => OakErrorKind::RepoAlreadyExists,
            OakError::NoCommits => OakErrorKind::NoCommits,
            OakError::InvalidHash(_) => OakErrorKind::InvalidHash,
            OakError::InvalidPath(_) => OakErrorKind::InvalidPath,
            OakError::BlobNotFound(_) => OakErrorKind::BlobNotFound,
            OakError::CommitNotFound(_) => OakErrorKind::CommitNotFound,
            OakError::BranchNotFound(_) => OakErrorKind::BranchNotFound,
            OakError::BranchAlreadyExists(_) => OakErrorKind::BranchAlreadyExists,
            OakError::BranchClosed(_) => OakErrorKind::BranchClosed,
            OakError::NoEdits => OakErrorKind::NoEdits,
            OakError::ManifestNotFound(_) => OakErrorKind::ManifestNotFound,
            OakError::UncommittedChanges => OakErrorKind::UncommittedChanges,
            OakError::WorktreeCaptureRaced { .. } => OakErrorKind::WorktreeCaptureRaced,
            OakError::UnsupportedChangeCaptureFormat(_) => {
                OakErrorKind::UnsupportedChangeCaptureFormat
            }
            OakError::ChangeCaptureScopeExcluded { .. } => OakErrorKind::ChangeCaptureScopeExcluded,
            OakError::CapturedBlobIntegrity { .. } => OakErrorKind::CapturedBlobIntegrity,
            OakError::CapturedTreeIntegrity { .. } => OakErrorKind::CapturedTreeIntegrity,
            OakError::ChangeCaptureNotFound { .. } => OakErrorKind::ChangeCaptureNotFound,
            OakError::ChangeCaptureIntegrity { .. } => OakErrorKind::ChangeCaptureIntegrity,
            OakError::DirtyWorkingTree(_) => OakErrorKind::DirtyWorkingTree,
            OakError::ConflictDetected => OakErrorKind::ConflictDetected,
            OakError::LocalCommitsNotInRemoteHistory => {
                OakErrorKind::LocalCommitsNotInRemoteHistory
            }
            OakError::RemoteCommitsNotInLocalHistory => {
                OakErrorKind::RemoteCommitsNotInLocalHistory
            }
            OakError::RemoteNotConfigured => OakErrorKind::RemoteNotConfigured,
            OakError::RemoteMoved { .. } => OakErrorKind::RemoteMoved,
            OakError::RemoteRepoNotFound(_) => OakErrorKind::RemoteRepoNotFound,
            OakError::RemoteRepoAlreadyExists(_) => OakErrorKind::RemoteRepoAlreadyExists,
            OakError::MergeFailed(_) => OakErrorKind::MergeFailed,
            OakError::IncompleteAncestry { .. } => OakErrorKind::IncompleteAncestry,
            OakError::IncompleteManifestData { .. } => OakErrorKind::IncompleteManifestData,
            OakError::IncompleteCommitData { .. } => OakErrorKind::IncompleteCommitData,
            OakError::IncompleteBlobData { .. } => OakErrorKind::IncompleteBlobData,
            OakError::NoVerifiedCommonAncestor { .. } => OakErrorKind::NoVerifiedCommonAncestor,
            OakError::MergeConflict(_) => OakErrorKind::MergeConflict,
            OakError::MergeInvariantViolation { .. } => OakErrorKind::MergeInvariantViolation,
            OakError::MergePredictionUncertified { .. } => OakErrorKind::MergePredictionUncertified,
            OakError::IntegrityInconclusive { .. } => OakErrorKind::IntegrityInconclusive,
            OakError::CloneAdmissionTokenTooLarge { .. } => {
                OakErrorKind::CloneAdmissionTokenTooLarge
            }
            OakError::IntegrityAdmissionBusy(_) => OakErrorKind::IntegrityAdmissionBusy,
            OakError::MergeInProgress => OakErrorKind::MergeInProgress,
            OakError::InterruptedWorktreeUpdate { .. } => OakErrorKind::InterruptedWorktreeUpdate,
            OakError::LocalChangesWouldBeOverwritten { .. } => {
                OakErrorKind::LocalChangesWouldBeOverwritten
            }
            OakError::NoMergeInProgress => OakErrorKind::NoMergeInProgress,
            OakError::Io(_) => OakErrorKind::Io,
            OakError::Database(_) => OakErrorKind::Database,
            OakError::Serialization(_) => OakErrorKind::Serialization,
            OakError::Http(_) => OakErrorKind::Http,
            OakError::Server(_) => OakErrorKind::Server,
            OakError::PublicationUnconfirmed { .. } => OakErrorKind::PublicationUnconfirmed,
            OakError::AuthRequired { .. } => OakErrorKind::AuthRequired,
            OakError::R2(_) => OakErrorKind::R2,
            OakError::Git(_) => OakErrorKind::Git,
            OakError::Config(_) => OakErrorKind::Config,
            OakError::FinishPreflight(_) => OakErrorKind::FinishPreflight,
            OakError::FinishPhaseFailed(_) => OakErrorKind::FinishPhaseFailed,
            OakError::CommitPhaseFailed(_) => OakErrorKind::CommitPhaseFailed,
            OakError::RepoLocked => OakErrorKind::RepoLocked,
            OakError::Unsupported { .. } => OakErrorKind::Unsupported,
            OakError::GroupNotFound(_) => OakErrorKind::GroupNotFound,
            OakError::InvalidArgument(_) => OakErrorKind::InvalidArgument,
            OakError::Usage(_) => OakErrorKind::Usage,
            OakError::DetachedHeadCommit(_) => OakErrorKind::DetachedHeadCommit,
            OakError::NoSyncInProgress => OakErrorKind::NoSyncInProgress,
            OakError::TrunkIsServerOnly(_) => OakErrorKind::TrunkIsServerOnly,
            OakError::NotLoggedIn { .. } => OakErrorKind::NotLoggedIn,
            OakError::RepoNotLinked(_) => OakErrorKind::RepoNotLinked,
            OakError::FeedbackFailed { .. } => OakErrorKind::FeedbackFailed,
            OakError::RootRefused { .. } => OakErrorKind::RootRefused,
            OakError::RestrictedContent(_) => OakErrorKind::RestrictedContent,
            OakError::DescriptionStale { .. } => OakErrorKind::DescriptionStale,
            OakError::ApprovalRefused { .. } => OakErrorKind::ApprovalRefused,
            OakError::PushCapabilityUnavailable { .. } => OakErrorKind::PushCapabilityUnavailable,
            OakError::PathOutsideView { .. } => OakErrorKind::PathOutsideView,
        }
    }
}

#[cfg(test)]
mod contract_vocabulary_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn error_codes_are_unique_specific_snake_case() {
        let mut seen = HashSet::new();
        for code in ErrorCode::ALL {
            let wire = code.as_str();
            assert!(seen.insert(wire), "duplicate error code {wire}");
            assert!(
                !matches!(wire, "error" | "generic" | "unknown" | "failure" | "failed"),
                "{wire} is a generic code; every failure needs a specific one"
            );
            assert!(
                wire.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{wire} is not snake_case"
            );
        }
    }

    /// The server's approval refusal codes (oakspace
    /// `crates/oak-web/src/server/api/approvals.rs`, every `err(_, CODE, _)`
    /// and `code:`), pinned: each must pass through with its exact string.
    #[test]
    fn every_server_approval_reason_keeps_its_exact_code() {
        const SERVER_TABLE: &[&str] = &[
            "boundary_not_touched",
            "boundary_outside_view",
            "branch_closed",
            "branch_moved",
            "internal",
            "not_an_owner",
            "not_found",
            "nothing_to_approve",
            "review_key_required",
            "self_approval",
            "service_account",
            "view",
        ];
        for reason in SERVER_TABLE {
            assert_eq!(
                ErrorCode::from_approval_reason(reason).map(ErrorCode::as_str),
                Some(*reason)
            );
        }
        assert_eq!(
            ErrorCode::from_approval_reason("approvals_required"),
            Some(ErrorCode::ApprovalsRequired)
        );
        assert_eq!(
            ErrorCode::from_approval_reason("a_newer_servers_reason"),
            None
        );
    }

    #[test]
    fn exit_codes_are_the_documented_table() {
        let codes: Vec<i32> = ExitCode::ALL.iter().map(|code| code.code()).collect();
        assert_eq!(codes, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn every_kind_is_listed_once() {
        let unique: HashSet<_> = OakErrorKind::ALL.iter().collect();
        assert_eq!(unique.len(), OakErrorKind::ALL.len());
    }
}
